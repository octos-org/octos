//! `octos doctor` — flutter-doctor-style local diagnostics.
//!
//! Runs the shared, product-agnostic checks from `octos-diagnostics` against an
//! octos-server [`ProductSpec`] and renders them through the shared [`Report`]:
//! one line per check (`[✓]` pass / `[!]` warn / `[✗]` fail), grouped by
//! category, each non-pass line followed by an indented `→ fix:` action,
//! closing with a one-line summary. `--json` emits the support bundle;
//! `--verbose` adds resolved paths/versions; `--strict` promotes warnings to
//! failures.
//!
//! Stage 1: binary/install-method/on-path/shadow, terminal, config+data
//! writability, and a structural protocol-skew check against the server's
//! compiled-in capabilities.
//!
//! Stage 2 adds a `Network` category (behind octos-diagnostics' `github`
//! feature, which octos-cli enables): GitHub reachability via the shared
//! `reachability()` and a best-effort newer-release check via `update_check`.
//! Both are advisory — a network/API failure WARNs, never FAILs, so `doctor`
//! works offline. The LIVE-WS `config/capabilities/list` probe for protocol
//! skew remains a documented `// TODO Stage 2.5` (it needs a client WS
//! connection).
//!
//! **Stage 3 (this file) adds the octos-specific health surface** — the
//! checks that answer "why doesn't octos work on THIS machine":
//! - `Config`: parse the resolved `config.json` (config is hand-edited now —
//!   a typo is the #1 support case) and surface the exact serde error.
//! - `Provider & auth`: resolve the configured provider, verify an API key is
//!   resolvable through the real chain (auth store → `env_vars`/keychain →
//!   process env; never printed), and probe the LLM endpoint over HTTP.
//! - `Profiles`: enumerate `profiles/*.json`, flag profiles without an LLM
//!   selection.
//! - `Stores & data`: probe the OS file lock on `admin_audit.redb` /
//!   per-profile `episodes.redb` to detect a live holder (a second
//!   `octos serve` on one data-dir fails exactly there — #1666) and report
//!   session/ledger disk usage. The probe never opens the database itself
//!   (see `store_lock_check` — `redb::Database::open` would stamp/repair).
//! - `Sandbox`: which backend `SandboxMode::Auto` selects, via the runtime's
//!   own probes (`octos_agent::sandbox::auto_sandbox_kind`).
//! - `Skills` / `MCP` / `Channels`: discovered skill manifests, MCP stdio
//!   command PATH-resolution, configured gateway channels.
//! - `Sessions` (Stage 4): a CONTENT-FREE inventory per store — counts,
//!   total size, newest/oldest age, transcripts near octos-bus's 10 MiB
//!   write cap, and transcripts whose final line no longer parses (the
//!   crash-mid-write signature that breaks resume). The tail probe parses
//!   and immediately discards; no transcript content reaches the report.
//!
//! Stage-3 contract: octos state is never created, migrated, or modified, and
//! no secret value reaches the report or the JSON bundle (env var NAMES only;
//! quoted literals are redacted out of parse errors; URLs are stripped of
//! userinfo/query for display AND before probing). Documented exceptions to
//! "no side effects", each mirroring what octos itself does on startup: the
//! API-key check runs the REAL resolver, which may re-`chmod 0600` the auth
//! store and resolve keychain-backed `env_vars` via the OS keychain; the
//! sandbox check runs the runtime's own availability probes (on Linux that
//! executes `bwrap --version`); the store lock probe holds a shared advisory
//! lock for microseconds. Network probes (provider endpoint) run only under
//! the same `with_network` gate as Stage 2 so unit tests stay offline and
//! deterministic. A `Notes` block above the report surfaces every non-pass
//! row first so problems never hide in a long green list.

use std::path::{Path, PathBuf};

use clap::Args;
use eyre::Result;
use octos_core::ui_protocol::{
    UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2, UI_PROTOCOL_KNOWN_FEATURES, UI_PROTOCOL_V1,
    UiProtocolCapabilities,
};
use octos_diagnostics::{
    Check, CheckStatus, InstallMethod, LocatedBinaries, ProductSpec, Reachability, Report,
    UpdatePlan, config_writability_check, data_writability_check, detect, locate, on_path_check,
    protocol_skew_check, reachability, shadow_check, terminal_checks, update_check,
};

use super::Executable;

const CAT_BINARY: &str = "Binary & version";
const CAT_INSTALLS: &str = "Installations";
const CAT_NETWORK: &str = "Network";
const CAT_CONFIG: &str = "Config";
const CAT_PROVIDER: &str = "Provider & auth";
const CAT_PROFILES: &str = "Profiles";
const CAT_STORES: &str = "Stores & data";
const CAT_SANDBOX: &str = "Sandbox";
const CAT_SKILLS: &str = "Skills";
const CAT_MCP: &str = "MCP";
const CAT_CHANNELS: &str = "Channels";
const CAT_SESSIONS: &str = "Sessions";

/// Per-profile rows (and per-profile store checks) are bounded so a fleet
/// data-dir with hundreds of profiles cannot flood the report.
const MAX_PROFILE_ROWS: usize = 8;
/// Session/ledger disk usage above this gets a WARN with a cleanup hint
/// (below it the usage is still reported as the check's value).
const DISK_WARN_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Hard cap on filesystem entries visited by the disk-usage walk so a
/// pathological tree cannot stall doctor.
const DISK_WALK_MAX_ENTRIES: usize = 100_000;
/// Sessions at ≥80% of `octos-bus`'s `MAX_SESSION_FILE_SIZE` (10 MiB) get
/// flagged before writes start failing at the cap.
const SESSION_NEAR_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes read from a transcript's TAIL for the content-free integrity probe
/// (parse the final line, discard it). Bounds I/O per session file.
const SESSION_TAIL_PROBE_BYTES: u64 = 64 * 1024;
/// Session files examined per store (metadata + tail probe each).
const MAX_SESSION_FILES: usize = 256;

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
            print!("{}", render_notes(&report));
            print!("{}", report.render(self.verbose, self.strict));
        }
        std::process::exit(report.exit_code(self.strict));
    }
}

/// A `Notes` block printed ABOVE the full report: one line per non-pass check
/// so problems surface first instead of hiding inside a long green list
/// (codex-doctor's leading notes section, done client-side so the shared
/// renderer stays untouched). Empty when everything passes.
fn render_notes(report: &Report) -> String {
    let flagged: Vec<&Check> = report
        .checks
        .iter()
        .filter(|check| check.status != CheckStatus::Pass)
        .collect();
    if flagged.is_empty() {
        return String::new();
    }
    let mut out = String::from("Notes\n");
    for check in flagged {
        let glyph = match check.status {
            CheckStatus::Fail => "✗",
            _ => "!",
        };
        out.push_str(&format!(
            "  {glyph} {} — {}\n",
            check.name,
            check.detail.as_str()
        ));
    }
    out.push('\n');
    out
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

    // --- Installations (every octos + octoscode copy, with versions) --------
    // Parity with `octoscode doctor`'s Installations section: enumerate BOTH
    // binaries across PATH + the known install dirs so duplicate / mismatched
    // installs are visible from either doctor.
    report.extend(installations_checks(&spec));

    // --- Config / data-dir context ------------------------------------------
    // Resolve the REAL config_home (~/.config/octos by default) and data_dir
    // (~/.octos) via the canonical resolver so doctor reports what octos
    // actually reads/writes. Read-only here: no migrations, no dir creation.
    let ctx = crate::config_context::resolve_config_context(cmd.data_dir.as_deref());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // --- Stage 3: Config parse ----------------------------------------------
    // The hand-edited config.json is parsed and its exact error surfaced.
    // The EFFECTIVE config (defaults when no file) feeds the provider /
    // profiles / stores / skills / MCP / channels checks below; when the file
    // is broken those checks are skipped (a degraded roster, like codex
    // doctor) rather than reporting against defaults that octos won't run
    // with either.
    let config_path = crate::config::resolve_config_file_path(&cwd, &ctx, None);
    let (config_check_row, effective_config) = config_parse_check(&config_path, &cwd, &ctx);
    report.push(config_check_row);

    if let Some(config) = &effective_config {
        // --- Stage 3: Provider & auth ----------------------------------------
        report.extend(provider_checks(config, with_network));
        // --- Stage 3: Profiles ------------------------------------------------
        let (profiles, profiles_dir_error) = load_profiles(&ctx.data_dir);
        if let Some(error) = profiles_dir_error {
            report.push(Check::warn(
                CAT_PROFILES,
                "profiles",
                format!("profiles dir exists but cannot be listed: {error}"),
                "check the directory's permissions/ownership",
            ));
        }
        report.extend(profile_checks(&profiles));
        // --- Stage 3: Stores & data -------------------------------------------
        report.extend(store_checks(&ctx.data_dir, &profiles));
        // --- Stage 4: Sessions (content-free inventory) -------------------------
        report.extend(session_checks(&ctx.data_dir, &profiles));
        // --- Stage 3: Skills / MCP / Channels ----------------------------------
        report.push(skills_check(&ctx.data_dir));
        report.extend(mcp_checks(config));
        report.push(channels_check(config));
    }

    // --- Stage 3: Sandbox (config-independent) -------------------------------
    report.push(sandbox_check());

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

    // --- Config & data (writability) ----------------------------------------
    report.push(config_writability_check(&ctx.config_home));
    report.push(data_writability_check(&ctx.data_dir));

    // --- Backend / protocol skew ------------------------------------------
    // The server's own compiled-in capabilities are authoritative for the
    // structural skew check. `first_server_slice()` advertises the protocol's
    // no-header compatibility baseline. `projection.envelope.v2` is known but
    // strictly opt-in, so exclude it here; otherwise doctor would mistake the
    // deliberately absent default advertisement for protocol skew.
    // TODO Stage 2.5: replace the compiled-in caps with a LIVE WS
    // `config/capabilities/list` probe against a configured/running server (it
    // needs a client WS connection, deliberately out of Stage 2 scope). Until
    // then the compiled-in `protocol_skew_check` is authoritative.
    let server_caps = UiProtocolCapabilities::first_server_slice();
    report.push(protocol_skew_check(
        &server_caps,
        UI_PROTOCOL_KNOWN_FEATURES
            .iter()
            .copied()
            .filter(|feature| *feature != UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
    ));

    Ok(report)
}

// ---------------------------------------------------------------------------
// Installations — every octos + octoscode on the machine, with versions
// ---------------------------------------------------------------------------

/// Parity with `octoscode doctor`'s Installations section: enumerate every
/// octos AND octoscode copy (across `$PATH`, Homebrew, cargo, the shell
/// installer's `~/.local/bin`, and octoscode's `~/.octos/bin` auto-install dir),
/// with each copy's `--version` + inferred install method — so duplicate /
/// mismatched installs are visible from `octos doctor` too, not just the TUI's.
fn installations_checks(octos: &ProductSpec) -> Vec<Check> {
    let mut checks = vec![
        installs_check("octos", &locate_with_octos_bin(octos)),
        installs_check("octoscode", &locate(&octoscode_spec())),
    ];
    // The client was renamed octos-tui -> octoscode. Enumerating only the new
    // name would report "none found" to anyone who has not upgraded yet —
    // wrong, and worst for exactly the user who needs `doctor` to explain
    // things. Look for the old binary too, and surface it ONLY when a copy is
    // actually present so the section does not grow a permanent empty row.
    // Drop this once the rename has settled.
    let legacy = locate(&octoscode_legacy_spec());
    if !install_rows(&legacy).is_empty() {
        checks.push(installs_check("octos-tui (legacy name)", &legacy));
    }
    checks
}

/// Minimal spec for LOCATING the octoscode client binary — only `binary_name`
/// matters for enumeration; the rest are placeholders.
fn octoscode_spec() -> ProductSpec {
    ProductSpec::new(
        "octoscode",
        "octoscode",
        "0.0.0",
        "octos-org/octoscode",
        "octoscode",
    )
}

/// Pre-rename spec, so a not-yet-upgraded `octos-tui` copy is still found.
/// See the note in [`installations_checks`].
fn octoscode_legacy_spec() -> ProductSpec {
    ProductSpec::new(
        "octos-tui",
        "octos-tui",
        "0.0.0",
        "octos-org/octos-tui",
        "octos-tui",
    )
}

/// `locate()` scans PATH + Homebrew/cargo/`~/.local/bin`, but octoscode's
/// auto-installer drops `octos` into `~/.octos/bin`, off both — add it (deduped
/// by canonical path).
fn locate_with_octos_bin(spec: &ProductSpec) -> LocatedBinaries {
    let mut located = locate(spec);
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = Path::new(&home)
            .join(".octos")
            .join("bin")
            .join(spec.binary_file_name());
        if candidate.is_file() {
            let canonical = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            let already = located
                .all()
                .iter()
                .any(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == canonical);
            if !already {
                located.off_path.push(candidate);
            }
        }
    }
    located
}

/// Best-effort install-method guess from a binary's on-disk path.
fn install_method_for_path(path: &Path) -> &'static str {
    let p = path.to_string_lossy();
    if p.contains("/.cargo/bin/") {
        "cargo"
    } else if p.contains("node_modules") {
        "npm"
    } else if p.contains("/homebrew/") || p.contains("/Cellar/") || p.starts_with("/usr/local/") {
        "brew"
    } else if p.contains("/.octos/bin/") {
        "octoscode auto-install"
    } else if p.contains("/.local/bin/") {
        "shell installer"
    } else if p.starts_with("/usr/bin/") || p.starts_with("/bin/") {
        "system"
    } else {
        "unknown"
    }
}

/// Run `<path> --version` and return its first non-empty line, or `None`.
fn probe_version(path: &Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// One display row per located binary: `<path> [<method>, on/off PATH] → <version>`.
fn install_rows(located: &LocatedBinaries) -> Vec<String> {
    located
        .all()
        .iter()
        .map(|p| {
            let method = install_method_for_path(p);
            let on = if located.on_path.contains(p) {
                "on PATH"
            } else {
                "off PATH"
            };
            let version = probe_version(p).unwrap_or_else(|| "no --version".to_string());
            format!("{} [{method}, {on}] → {version}", p.display())
        })
        .collect()
}

/// PASS on exactly one install, WARN on duplicates (naming the extras to
/// remove) or on none found.
fn installs_check(name: &str, located: &LocatedBinaries) -> Check {
    let rows = install_rows(located);
    match rows.len() {
        0 => Check::warn(
            CAT_INSTALLS,
            format!("{name} installs"),
            "none found on $PATH or known install dirs",
            format!("install {name}"),
        ),
        1 => Check::pass(CAT_INSTALLS, format!("{name} installs"), rows[0].clone()),
        n => Check::warn(
            CAT_INSTALLS,
            format!("{name} installs"),
            format!("{n} installs found — the first on $PATH wins; the rest can confuse updates"),
            format!(
                "remove the copies you don't want: {}",
                rows[1..].join(" ; ")
            ),
        )
        .with_value(rows.join(" | ")),
    }
}

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

// ---------------------------------------------------------------------------
// Stage 3 checks — all READ-ONLY (no dir creation, no migrations, no spawns).
// ---------------------------------------------------------------------------

/// Parse the resolved startup config and return the EFFECTIVE config for the
/// downstream checks. Distinguishes three cases: no file (defaults — pass with
/// a note), parsed (pass with a provider/model summary), broken (FAIL with the
/// exact error and the file to edit — downstream config-dependent checks are
/// skipped, mirroring the fact that octos itself won't run with that file).
fn config_parse_check(
    config_path: &Path,
    cwd: &Path,
    ctx: &crate::config_context::ConfigContext,
) -> (Check, Option<crate::config::Config>) {
    let exists = config_path.is_file();
    match crate::config::Config::load_with_context(cwd, ctx) {
        Ok(config) if exists => {
            let provider = config.provider.as_deref().unwrap_or("(unset)");
            let model = config.model.as_deref().unwrap_or("(unset)");
            let api_type = config.api_type.as_deref().unwrap_or("(default)");
            let check = Check::pass(
                CAT_CONFIG,
                "config.json",
                format!("parsed — provider {provider} · model {model} · api_type {api_type}"),
            )
            .with_value(config_path.display().to_string());
            (check, Some(config))
        }
        Ok(config) => {
            let check = Check::pass(
                CAT_CONFIG,
                "config.json",
                "no config file — built-in defaults in effect (run `octos init` for a quickstart)",
            )
            .with_value(config_path.display().to_string());
            (check, Some(config))
        }
        Err(error) => {
            let check = Check::fail(
                CAT_CONFIG,
                "config.json",
                format!(
                    "failed to load: {}",
                    redact_quoted_literals(&format!("{error:#}"))
                ),
                format!(
                    "edit {} (inspect with `octos config show`), then rerun octos doctor",
                    config_path.display()
                ),
            )
            .with_value(config_path.display().to_string());
            (check, None)
        }
    }
}

/// Provider resolution + API-key resolvability + endpoint reachability.
///
/// Provider metadata (canonical name, aliases, key env var, default endpoint,
/// keyless-ness) comes from [`octos_llm::registry`] — the SAME source the
/// runtime uses — so a `provider: "Ollama"` alias or Vertex's
/// `VERTEX_SA_JSON` is reported exactly as the runtime resolves it (codex).
///
/// The key check runs the REAL resolution chain
/// ([`crate::config::Config::get_api_key_with_env`]: auth store →
/// `env_vars`/keychain → process env) and never prints the key or its length —
/// only the env var name it would resolve through. The endpoint probe is a
/// short-timeout GET with credentials STRIPPED (userinfo/query/fragment are
/// removed before the request — reqwest would otherwise turn URL userinfo
/// into a Basic Authorization header); any HTTP status counts as reachable
/// (401/404 still prove the endpoint answers — auth is the separate check
/// above), only transport errors warn. Probe runs only `with_network`.
fn provider_checks(config: &crate::config::Config, with_network: bool) -> Vec<Check> {
    let mut checks = Vec::new();

    let provider = config.provider.clone().or_else(|| {
        config
            .model
            .as_deref()
            .and_then(crate::config::detect_provider)
            .map(String::from)
    });
    let Some(provider) = provider else {
        checks.push(Check::warn(
            CAT_PROVIDER,
            "provider",
            "no LLM provider configured",
            "set \"provider\" (and \"model\") in config.json, or run `octos init`",
        ));
        return checks;
    };

    // Registry lookup: case-insensitive + alias-aware, same as the runtime.
    let entry = octos_llm::registry::lookup(&provider);
    let canonical = entry.map(|e| e.name).unwrap_or(provider.as_str());
    let endpoint = config
        .base_url
        .clone()
        .or_else(|| entry.and_then(|e| e.default_base_url.map(String::from)));
    let endpoint_display = endpoint.as_deref().map(sanitize_url_for_display);
    match entry {
        Some(_) => checks.push(
            Check::pass(CAT_PROVIDER, "provider", canonical).with_value(
                endpoint_display
                    .clone()
                    .unwrap_or_else(|| "provider-default endpoint".into()),
            ),
        ),
        // `custom` is a runtime special case (bring-your-own endpoint), not a
        // registry miss (codex r2).
        None if canonical == "custom" => checks.push(
            Check::pass(
                CAT_PROVIDER,
                "provider",
                "custom endpoint provider (config-driven base_url/api_type)",
            )
            .with_value(
                endpoint_display
                    .clone()
                    .unwrap_or_else(|| "base_url not set".into()),
            ),
        ),
        None => checks.push(Check::warn(
            CAT_PROVIDER,
            "provider",
            format!("{provider} is not a registered provider"),
            "check the name against `octos init`'s provider list (custom endpoints: keep it + set base_url/api_type)",
        )),
    }

    // API key resolvability (redacted: env var NAME only, never the value).
    // `api_key_env: None` in the registry marks a genuinely keyless local
    // provider (ollama); an explicit config `api_key_env` always wins.
    let registry_env = entry.and_then(|e| e.api_key_env);
    let raw_env = config
        .api_key_env
        .clone()
        .or_else(|| registry_env.map(String::from))
        .unwrap_or_else(|| format!("{}_API_KEY", canonical.to_uppercase()));
    let env_name = display_env_name(raw_env.clone());
    // When the configured name was implausible (a pasted secret), it must
    // also be scrubbed out of the RESOLVER's error text below — the resolver
    // echoes the env var name it looked up (live-caught residual).
    let env_redacted = env_name != raw_env;
    if entry.is_some() && registry_env.is_none() && config.api_key_env.is_none() {
        checks.push(Check::pass(
            CAT_PROVIDER,
            "API key",
            format!("{canonical} is a local provider — no API key required"),
        ));
    } else {
        // Resolve exactly as the runtime does: pass the user's OWN
        // `api_key_env` config (None → the full chain INCLUDING the auth
        // store; a custom env var intentionally narrows it — codex r2:
        // always passing a derived name skipped the auth store and doctor
        // could fail while runtime works).
        match config.get_api_key_with_env(canonical, config.api_key_env.as_deref()) {
            Ok(_) => checks.push(
                Check::pass(CAT_PROVIDER, "API key", "resolved (redacted)").with_value(env_name),
            ),
            Err(error) => {
                // A provider that REQUIRES a base_url is a local/self-hosted
                // deployment (vllm) whose key is commonly optional; unknown /
                // custom providers may not need one either. Only the known
                // key-required cloud providers hard-FAIL (codex r2).
                let cloud_key_required = entry
                    .map(|e| e.api_key_env.is_some() && !e.requires_base_url)
                    .unwrap_or(false);
                let mut error_text = error.to_string();
                if env_redacted {
                    error_text = error_text.replace(&raw_env, "[redacted]");
                }
                let detail = format!("not resolvable: {error_text}");
                let fix = format!("run `octos auth login -p {canonical}`, or export {env_name}");
                checks.push(
                    if cloud_key_required {
                        Check::fail(CAT_PROVIDER, "API key", detail, fix)
                    } else {
                        Check::warn(
                            CAT_PROVIDER,
                            "API key",
                            format!("{detail} (may be optional for this provider)"),
                            fix,
                        )
                    }
                    .with_value(env_name),
                );
            }
        }
    }

    // Endpoint reachability (advisory; skipped offline/tests).
    if with_network {
        match endpoint {
            Some(url) => {
                let display = endpoint_display.unwrap_or_else(|| "(unparseable URL)".into());
                match probe_endpoint(&url) {
                    Ok(status) => checks.push(
                        Check::pass(
                            CAT_PROVIDER,
                            "endpoint reachable",
                            format!("HTTP {status} from the LLM endpoint"),
                        )
                        .with_value(display),
                    ),
                    Err(error) => checks.push(
                        Check::warn(
                            CAT_PROVIDER,
                            "endpoint reachable",
                            format!("could not reach {display}: {error}"),
                            "check network/proxy/DNS, or fix \"base_url\" in config.json",
                        )
                        .with_value(display),
                    ),
                }
            }
            None => checks.push(Check::pass(
                CAT_PROVIDER,
                "endpoint reachable",
                format!(
                    "no default endpoint known for {canonical} — set \"base_url\" to enable the probe"
                ),
            )),
        }
    }

    checks
}

/// Guard an env-var NAME before it is echoed into report values / fix lines:
/// a config mistake like `api_key_env: "sk-live-…"` (the KEY pasted where the
/// NAME belongs) must not put the secret in the support bundle (codex r2).
/// Plausible names — `[A-Za-z_][A-Za-z0-9_]*`, ≤64 chars — pass through.
fn display_env_name(candidate: String) -> String {
    let plausible = !candidate.is_empty()
        && candidate.len() <= 64
        && candidate
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plausible {
        candidate
    } else {
        "[api_key_env is not a valid env var name — redacted]".to_string()
    }
}

/// Redact the CONTENT of every double-quoted / backticked segment in an error
/// message, keeping its structure (error type, line/column). Serde type
/// errors quote the offending input literal — e.g. a config where a secret
/// landed in the wrong field would otherwise echo that secret into the report
/// and the JSON support bundle (codex).
fn redact_quoted_literals(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut inside: Option<char> = None;
    let mut escaped = false;
    for ch in message.chars() {
        match inside {
            None => {
                out.push(ch);
                if ch == '"' || ch == '`' {
                    inside = Some(ch);
                    escaped = false;
                    out.push_str("[redacted]");
                }
            }
            Some(open) => {
                // Honor backslash escapes INSIDE the literal: serde formats
                // strings with `{:?}`, so an embedded quote arrives as `\"`
                // and must not close the segment early (which would leak the
                // remainder — codex r2). Content chars are dropped either way.
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == open {
                    out.push(ch);
                    inside = None;
                }
            }
        }
    }
    out
}

/// Strip credentials and query/fragment from a URL for DISPLAY (report rows
/// and the JSON bundle): `https://user:pass@host/v1?key=tok` →
/// `https://host/v1`. Unparseable input falls back to scheme+"(unparseable
/// URL)" rather than echoing the raw string, which could embed a secret.
fn sanitize_url_for_display(raw: &str) -> String {
    match sanitized_http_url(raw) {
        Ok(url) => url.to_string(),
        Err(_) => "(non-HTTP or unparseable URL)".to_string(),
    }
}

/// Parse `raw` as an HTTP(S) URL with a host and strip userinfo, query, and
/// fragment. Anything else — including opaque schemes like `data:`, whose
/// "path" would carry the raw string verbatim — is rejected rather than
/// displayed or probed (codex r2).
fn sanitized_http_url(raw: &str) -> std::result::Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(raw).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("not an http(s) URL with a host".to_string());
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// Short-timeout GET with credentials STRIPPED: userinfo, query, and fragment
/// are removed before the request so a `https://user:pass@host` or
/// `?api_key=…` base_url never sends its secret (reqwest converts URL
/// userinfo into a Basic Authorization header). Any HTTP status is success
/// (the endpoint answered); transport failures and unparseable URLs are
/// errors.
fn probe_endpoint(url: &str) -> std::result::Result<u16, String> {
    let parsed = sanitized_http_url(url)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        // No redirect following: a redirect target could carry credentials
        // that would then be echoed through the error path (codex r2).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    client
        .get(parsed)
        .send()
        .map(|response| response.status().as_u16())
        .map_err(|error| error.to_string())
}

/// One discovered profile: id (file stem) + parse outcome. Reading is manual
/// (`fs::read_dir` on an EXISTING dir only) because `ProfileStore::open`
/// creates the profiles dir — doctor must not mutate.
type DiscoveredProfile = (
    String,
    std::result::Result<crate::profiles::UserProfile, String>,
);

/// Hard cap on directory entries examined per scan (profiles, skills) — the
/// display bound alone would not stop doctor from READING an unbounded dir.
const MAX_SCAN_ENTRIES: usize = 256;
/// Files above this size are reported unreadable instead of being loaded —
/// no profile/manifest JSON is legitimately this large.
const MAX_JSON_READ_BYTES: u64 = 1024 * 1024; // 1 MiB

/// Load `profiles/*.json` read-only. Returns the discovered profiles plus a
/// directory-level error when the dir EXISTS but cannot be listed — silently
/// treating that as "no profiles" would be a false-green diagnosis (codex).
fn load_profiles(data_dir: &Path) -> (Vec<DiscoveredProfile>, Option<String>) {
    let dir = data_dir.join("profiles");
    if !dir.exists() {
        return (Vec::new(), None);
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    // `take` BEFORE `flatten`: the cap must bound raw directory entries
    // (including error entries), not just the successful ones (codex r2).
    let mut profiles: Vec<DiscoveredProfile> = entries
        .take(MAX_SCAN_ENTRIES)
        .flatten()
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "json")
                && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
        })
        .map(|entry| {
            let id = entry
                .path()
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let oversized = entry
                .metadata()
                .map(|meta| meta.len() > MAX_JSON_READ_BYTES)
                .unwrap_or(false);
            let parsed = if oversized {
                Err(format!(
                    "file exceeds {} — not a plausible profile JSON",
                    human_bytes(MAX_JSON_READ_BYTES)
                ))
            } else {
                std::fs::read_to_string(entry.path())
                    .map_err(|error| error.to_string())
                    .and_then(|text| {
                        serde_json::from_str::<crate::profiles::UserProfile>(&text)
                            .map_err(|error| redact_quoted_literals(&error.to_string()))
                    })
            };
            (id, parsed)
        })
        .collect();
    profiles.sort_by(|a, b| a.0.cmp(&b.0));
    (profiles, None)
}

/// Profile inventory: count + per-profile LLM-selection presence (bounded).
fn profile_checks(profiles: &[DiscoveredProfile]) -> Vec<Check> {
    let mut checks = Vec::new();
    if profiles.is_empty() {
        checks.push(Check::pass(
            CAT_PROFILES,
            "profiles",
            "none yet — created by octoscode onboarding (or `octos serve` solo mode)",
        ));
        return checks;
    }
    checks.push(Check::pass(
        CAT_PROFILES,
        "profiles",
        if profiles.len() >= MAX_SCAN_ENTRIES {
            format!(
                "{} profile(s) found (directory scan capped at {MAX_SCAN_ENTRIES})",
                profiles.len()
            )
        } else {
            format!("{} profile(s) found", profiles.len())
        },
    ));
    for (id, parsed) in profiles.iter().take(MAX_PROFILE_ROWS) {
        match parsed {
            Err(error) => checks.push(Check::warn(
                CAT_PROFILES,
                format!("profile {id}"),
                format!("unreadable profile JSON: {error}"),
                format!("repair or remove profiles/{id}.json"),
            )),
            Ok(profile) => {
                let llm = profile
                    .config
                    .llm
                    .as_ref()
                    .and_then(|llm| llm.primary.as_ref());
                match llm {
                    Some(primary) => {
                        let family = primary.family_id.as_deref().unwrap_or("?");
                        let model = primary.model_id.as_deref().unwrap_or("?");
                        let disabled = if profile.enabled { "" } else { " (disabled)" };
                        checks.push(Check::pass(
                            CAT_PROFILES,
                            format!("profile {id}"),
                            format!("llm {family}/{model}{disabled}"),
                        ));
                    }
                    None => checks.push(Check::warn(
                        CAT_PROFILES,
                        format!("profile {id}"),
                        "no LLM selection saved",
                        "finish onboarding for this profile (or edit its llm block)",
                    )),
                }
            }
        }
    }
    if profiles.len() > MAX_PROFILE_ROWS {
        checks.push(Check::pass(
            CAT_PROFILES,
            "profiles (more)",
            format!(
                "… and {} more (showing first {MAX_PROFILE_ROWS})",
                profiles.len() - MAX_PROFILE_ROWS
            ),
        ));
    }
    checks
}

/// Classify an EXISTING redb store's lock state WITHOUT opening it as a
/// database: healthy-and-unlocked, held by another octos process (the
/// single-writer lock — how a second `octos serve` on one data-dir dies,
/// #1666), or unreadable.
///
/// Deliberately NOT `redb::Database::open`: redb opens read-write, stamps its
/// recovery flag even on a clean open, and auto-repairs an unclean store —
/// all mutations, with unbounded recovery time (codex). Instead this takes
/// the same OS advisory file lock redb uses (flock/LockFileEx via `fs2`) in
/// SHARED, NON-BLOCKING mode on a READ-ONLY handle: a live holder makes the
/// try-lock fail with a typed `WouldBlock` (no string matching), a free store
/// grants it instantly, and nothing is ever written. The shared lock is
/// dropped immediately; the microseconds-wide window in which it could make a
/// concurrently-STARTING serve fail is documented, unavoidable for any
/// observer, and far smaller than holding a full database open.
fn store_lock_check(category: &'static str, name: String, path: &Path) -> Option<Check> {
    if !path.is_file() {
        return None;
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return Some(
                Check::warn(
                    category,
                    name,
                    format!("cannot read store file: {error}"),
                    "check the file's permissions/ownership",
                )
                .with_value(path.display().to_string()),
            );
        }
    };
    // Advisory file lock via `fs2` (flock / LockFileEx — the same OS
    // mechanism redb's single-writer lock uses), shared + non-blocking.
    // Fully-qualified trait calls: std 1.89 grew inherent methods of the same
    // names which would otherwise win resolution, and the workspace MSRV is
    // 1.85 (codex r2 P1). Contention is compared against the platform's
    // canonical contended errno rather than string/kind matching.
    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            Some(
                Check::pass(category, name, "present, not locked by another process")
                    .with_value(path.display().to_string()),
            )
        }
        Err(error)
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Some(
                Check::warn(
                    category,
                    name,
                    "in use by another octos process (an `octos serve` on this data-dir?)",
                    "expected while a serve runs; a SECOND serve on the same data-dir will fail to start",
                )
                .with_value(path.display().to_string()),
            )
        }
        Err(error) => Some(
            Check::warn(
                category,
                name,
                format!("lock state unknown: {error}"),
                "check the filesystem (network mounts may not support file locks)",
            )
            .with_value(path.display().to_string()),
        ),
    }
}

/// Store health + disk usage for the data dir.
fn store_checks(data_dir: &Path, profiles: &[DiscoveredProfile]) -> Vec<Check> {
    let mut checks = Vec::new();

    // Data-dir-root admin audit store (the two-serve lock point).
    match store_lock_check(
        CAT_STORES,
        "admin audit store".into(),
        &data_dir.join("admin_audit.redb"),
    ) {
        Some(check) => checks.push(check),
        None => checks.push(Check::pass(
            CAT_STORES,
            "admin audit store",
            "not created yet",
        )),
    }

    // Per-profile episode stores (bounded; silent when a profile has none).
    // The store base honors the profile's own `data_dir` override, exactly
    // like `ProfileStore::resolve_data_dir` — otherwise a relocated profile's
    // LIVE store would be invisible here (codex r2).
    let profile_base =
        |id: &str, parsed: &std::result::Result<crate::profiles::UserProfile, String>| {
            parsed
                .as_ref()
                .ok()
                .and_then(|profile| profile.data_dir.as_ref())
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("profiles").join(id).join("data"))
        };
    for (id, parsed) in profiles.iter().take(MAX_PROFILE_ROWS) {
        let path = profile_base(id, parsed).join("episodes.redb");
        if let Some(check) = store_lock_check(CAT_STORES, format!("episodes ({id})"), &path) {
            checks.push(check);
        }
    }

    // Session / ledger disk usage (transcripts, ui-protocol event ledgers,
    // context ledgers — global + per-profile + per-project stores are all
    // rooted under the data dir except `sessions_in_cwd` project stores,
    // which live in each project and are intentionally out of scope here).
    let mut roots: Vec<PathBuf> = vec![
        data_dir.join("sessions"),
        data_dir.join("ui-protocol"),
        data_dir.join("context_ledgers"),
        data_dir.join("users"),
    ];
    for (id, parsed) in profiles {
        let base = profile_base(id, parsed);
        roots.push(base.join("sessions"));
        roots.push(base.join("context_ledgers"));
        roots.push(base.join("users"));
    }
    let usage = disk_usage(&roots);
    let mut notes = String::new();
    if usage.capped {
        notes.push_str(" (walk capped — real usage is higher)");
    }
    if usage.unreadable {
        notes.push_str(" (some paths unreadable — measurement incomplete)");
    }
    let detail = format!(
        "{} across {} file(s){notes}",
        human_bytes(usage.bytes),
        usage.files
    );
    if usage.bytes > DISK_WARN_BYTES {
        checks.push(Check::warn(
            CAT_STORES,
            "sessions & ledgers on disk",
            detail,
            "prune old sessions/ledgers (`octos clean`) if this keeps growing",
        ));
    } else {
        checks.push(Check::pass(
            CAT_STORES,
            "sessions & ledgers on disk",
            detail,
        ));
    }

    checks
}

/// Content-free inventory of one session STORE (a root's `sessions/` +
/// `users/<base>/sessions/` dirs — the exact layout `SessionManager` writes).
/// Only counts, sizes, ages, and file stems (session keys) are collected;
/// transcript CONTENT is parsed for validity and immediately discarded, and
/// never reaches the report or the JSON support bundle.
#[derive(Default)]
struct SessionInventory {
    files: u64,
    bytes: u64,
    newest_age_secs: Option<u64>,
    oldest_age_secs: Option<u64>,
    /// Session keys (file stems) whose size is ≥ [`SESSION_NEAR_CAP_BYTES`]
    /// — writes fail outright at octos-bus's 10 MiB cap.
    near_cap: Vec<String>,
    /// Session keys whose final line does not parse as JSON (a truncated /
    /// corrupt tail — the usual crash-mid-write signature).
    corrupt_tail: Vec<String>,
    capped: bool,
    unreadable: bool,
    /// Some files had a future/unavailable mtime and are excluded from the
    /// newest/oldest range.
    ages_missing: bool,
}

/// Inventory `root/sessions/*.jsonl` + `root/users/<base>/sessions/*.jsonl`.
fn scan_session_store(root: &Path, inventory: &mut SessionInventory) {
    scan_session_dir(&root.join("sessions"), inventory);
    let users = root.join("users");
    if users.is_dir() {
        let Ok(entries) = std::fs::read_dir(&users) else {
            inventory.unreadable = true;
            return;
        };
        // Enumerate RAW entries so hitting the bound is observable — a
        // silent `take` would let user-dir #257 vanish while the row still
        // read as a complete inventory (codex).
        for (index, entry) in entries.enumerate() {
            if index >= MAX_SCAN_ENTRIES {
                inventory.capped = true;
                break;
            }
            let Ok(entry) = entry else {
                inventory.unreadable = true;
                continue;
            };
            // lstat-based: a symlinked user dir would walk OUTSIDE the store
            // (the disk walker doesn't follow links either).
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            scan_session_dir(&entry.path().join("sessions"), inventory);
        }
    }
}

fn scan_session_dir(dir: &Path, inventory: &mut SessionInventory) {
    if !dir.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            inventory.unreadable = true;
            return;
        }
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SESSION_FILES || inventory.files >= MAX_SESSION_FILES as u64 {
            inventory.capped = true;
            return;
        }
        let Ok(entry) = entry else {
            inventory.unreadable = true;
            continue;
        };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        // Skip SIDECARS (`<key>.tasks.jsonl` task ledgers, and any future
        // dotted suffix): session keys are fully percent-encoded
        // (`encode_path_component` encodes `.` as %2E), so a literal dot in
        // the stem can only be a sidecar — not a transcript, not governed by
        // the 10 MiB session cap, and it must not consume the scan budget or
        // trigger resume/fork advice (codex r2).
        if path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().contains('.'))
        {
            continue;
        }
        // lstat-based file-type check: symlinked "session files" would read
        // an unrelated target outside the store — skip them, matching the
        // disk walker's no-follow rule (codex).
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            inventory.unreadable = true;
            continue;
        };
        inventory.files += 1;
        inventory.bytes += meta.len();
        match meta
            .modified()
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        {
            Some(age) => {
                let secs = age.as_secs();
                inventory.newest_age_secs =
                    Some(inventory.newest_age_secs.map_or(secs, |n| n.min(secs)));
                inventory.oldest_age_secs =
                    Some(inventory.oldest_age_secs.map_or(secs, |o| o.max(secs)));
            }
            // Future or unavailable mtime: excluded from the range — say so
            // instead of presenting a partial range as complete (codex).
            None => inventory.ages_missing = true,
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if meta.len() >= SESSION_NEAR_CAP_BYTES {
            inventory.near_cap.push(stem.clone());
        }
        if meta.len() > 0 && session_tail_parses(&path, meta.len()) == Some(false) {
            inventory.corrupt_tail.push(stem);
        }
    }
}

/// Content-free integrity probe: read at most the final
/// [`SESSION_TAIL_PROBE_BYTES`] of the transcript and check that its LAST
/// line parses as JSON. Parsing goes straight from the raw BYTES into
/// `serde::de::IgnoredAny` (byte-faithful: invalid UTF-8 fails exactly like
/// the runtime's `read_to_string` would, and nothing from the transcript is
/// retained or reported). Returns:
/// - `Some(true)`  — final line parses (or the file is whitespace-only)
/// - `Some(false)` — final line is definitively corrupt
/// - `None`        — VERDICT UNKNOWN: the window has no newline before the
///   final line and the file extends beyond the window, so that "line" may
///   be the mere suffix of a legitimately huge (>64 KiB) record. Flagging it
///   would tell the user to delete a healthy session (codex) — callers must
///   treat `None` as not-corrupt.
///
/// An unreadable file is `Some(false)` — it would fail at resume too.
fn session_tail_parses(path: &Path, len: u64) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Some(false);
    };
    let start = len.saturating_sub(SESSION_TAIL_PROBE_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Some(false);
    }
    let mut tail = Vec::with_capacity((len - start) as usize);
    if file
        .take(SESSION_TAIL_PROBE_BYTES)
        .read_to_end(&mut tail)
        .is_err()
    {
        return Some(false);
    }
    // Trim trailing whitespace (covers trailing newline / CRLF) — JSON's
    // whitespace set ONLY (space/tab/CR/LF): trimming e.g. a form-feed would
    // make a tail pass that raw serde_json rejects (codex r2).
    let trimmed_end = tail
        .iter()
        .rposition(|&byte| !is_json_whitespace(byte))
        .map(|pos| &tail[..=pos]);
    let Some(content) = trimmed_end else {
        // Whitespace-only window: nothing to validate.
        return Some(true);
    };
    let line_start = match content.iter().rposition(|&byte| byte == b'\n') {
        Some(pos) => pos + 1,
        // No newline in the whole window: if the file extends beyond the
        // window, this is a fragment of a longer record — verdict unknown.
        None if start > 0 => return None,
        None => 0,
    };
    let line = trim_json_whitespace(&content[line_start..]);
    Some(serde_json::from_slice::<serde::de::IgnoredAny>(line).is_ok())
}

/// JSON whitespace (RFC 8259): space, tab, CR, LF — deliberately NOT the full
/// ASCII whitespace set.
fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn trim_json_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| !is_json_whitespace(byte))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&byte| !is_json_whitespace(byte))
        .map_or(start, |pos| pos + 1);
    &bytes[start..end]
}

fn humanize_age(secs: u64) -> String {
    match secs {
        0..=59 => "<1m".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// One inventory row per session store: the serve-level store at the data-dir
/// root plus each profile's store (same roots the disk-usage walk covers;
/// per-project `sessions_in_cwd` stores live in unknown project dirs and stay
/// out of scope). Rows are informational; unparseable tails and near-cap
/// transcripts WARN with the offending session keys (bounded).
fn session_checks(data_dir: &Path, profiles: &[DiscoveredProfile]) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut stores: Vec<(String, PathBuf)> = vec![("server".to_string(), data_dir.to_path_buf())];
    for (id, parsed) in profiles.iter().take(MAX_PROFILE_ROWS) {
        let base = parsed
            .as_ref()
            .ok()
            .and_then(|profile| profile.data_dir.as_ref())
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("profiles").join(id).join("data"));
        // Dedup: a profile whose data_dir override points AT (or repeats) an
        // already-listed root would inventory the same store twice (codex).
        if stores.iter().any(|(_, existing)| existing == &base) {
            continue;
        }
        stores.push((id.clone(), base));
    }

    for (label, root) in stores {
        let mut inventory = SessionInventory::default();
        scan_session_store(&root, &mut inventory);
        if inventory.files == 0 {
            // An empty-LOOKING store that was actually unreadable/truncated
            // must not vanish into a green "none stored yet" (codex).
            if inventory.unreadable || inventory.capped {
                checks.push(Check::warn(
                    CAT_SESSIONS,
                    format!("sessions ({label})"),
                    "store could not be fully scanned (unreadable or truncated listing)",
                    "check directory permissions under the store root",
                ));
            }
            continue;
        }
        let ages = match (inventory.newest_age_secs, inventory.oldest_age_secs) {
            (Some(newest), Some(oldest)) => format!(
                " · newest {} ago · oldest {} ago",
                humanize_age(newest),
                humanize_age(oldest)
            ),
            _ => String::new(),
        };
        let mut notes = String::new();
        if inventory.capped {
            notes.push_str(&format!(" (scan capped at {MAX_SESSION_FILES})"));
        }
        if inventory.unreadable {
            notes.push_str(" (some paths unreadable)");
        }
        if inventory.ages_missing {
            notes.push_str(" (some ages unavailable)");
        }
        let summary = format!(
            "{} session(s) · {}{ages}{notes}",
            inventory.files,
            human_bytes(inventory.bytes)
        );
        if inventory.corrupt_tail.is_empty() && inventory.near_cap.is_empty() {
            checks.push(Check::pass(
                CAT_SESSIONS,
                format!("sessions ({label})"),
                summary,
            ));
        } else {
            let mut problems = Vec::new();
            let mut fixes = Vec::new();
            if !inventory.corrupt_tail.is_empty() {
                let total = inventory.corrupt_tail.len();
                let mut names = inventory.corrupt_tail.clone();
                names.sort();
                names.truncate(3);
                let suffix = if total > 3 { ", …" } else { "" };
                problems.push(format!(
                    "{total} with unparseable tail: {}{suffix}",
                    names.join(", ")
                ));
                fixes.push("unparseable tails break resume — back up + remove those files");
            }
            if !inventory.near_cap.is_empty() {
                problems.push(format!(
                    "{} near the 10 MiB session cap",
                    inventory.near_cap.len()
                ));
                fixes.push("fork near-cap sessions (`/new`) before writes start failing");
            }
            checks.push(Check::warn(
                CAT_SESSIONS,
                format!("sessions ({label})"),
                format!("{summary} — {}", problems.join("; ")),
                fixes.join("; "),
            ));
        }
    }
    if profiles.len() > MAX_PROFILE_ROWS {
        checks.push(Check::pass(
            CAT_SESSIONS,
            "sessions (more)",
            format!(
                "… {} more profile store(s) not scanned (showing first {MAX_PROFILE_ROWS})",
                profiles.len() - MAX_PROFILE_ROWS
            ),
        ));
    }
    // "none stored yet" only when NOTHING else was said: with unscanned
    // profile stores disclosed above, claiming "none" would contradict the
    // disclosure — an unscanned store may well contain sessions (codex r2).
    if checks.is_empty() {
        checks.push(Check::pass(CAT_SESSIONS, "sessions", "none stored yet"));
    }
    checks
}

/// Outcome of the bounded disk walk. `capped`/`unreadable` make the summary
/// honest: a truncated or partially-unreadable walk must not read as a
/// complete measurement (codex false-green).
struct DiskUsage {
    bytes: u64,
    files: u64,
    capped: bool,
    unreadable: bool,
}

/// Sum file sizes under `roots` (recursive), visiting at most
/// [`DISK_WALK_MAX_ENTRIES`] entries. Symlinks are not followed.
fn disk_usage(roots: &[PathBuf]) -> DiskUsage {
    let mut usage = DiskUsage {
        bytes: 0,
        files: 0,
        capped: false,
        unreadable: false,
    };
    let mut visited = 0usize;
    let mut stack: Vec<PathBuf> = roots.iter().filter(|r| r.exists()).cloned().collect();
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                usage.unreadable = true;
                continue;
            }
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > DISK_WALK_MAX_ENTRIES {
                usage.capped = true;
                return usage;
            }
            let Ok(file_type) = entry.file_type() else {
                usage.unreadable = true;
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                usage.files += 1;
                usage.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
            // Symlinks are intentionally not followed.
        }
    }
    usage
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// The PATH dirs used for binary-availability probes (sandbox backends, MCP
/// stdio commands). Split out so tests can inject a synthetic PATH.
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

/// Whether `name` resolves to an executable file in `dirs` (no execution).
fn binary_on_path(dirs: &[PathBuf], name: &str) -> bool {
    let mut candidates = vec![name.to_string()];
    if cfg!(windows) {
        candidates.push(format!("{name}.exe"));
        candidates.push(format!("{name}.cmd"));
    }
    dirs.iter()
        .any(|dir| candidates.iter().any(|cand| dir.join(cand).is_file()))
}

/// Which backend `SandboxMode::Auto` selects on this host — delegated to
/// [`octos_agent::sandbox::auto_sandbox_kind`], the SAME probes the runtime
/// runs (a PATH-existence guess diverged from reality: runtime `bwrap_works`
/// actually executes `bwrap --version`, and the Linux-container / Windows
/// AppContainer fallbacks were invisible to a which-scan — codex). On Linux
/// this therefore may briefly run `bwrap --version`, the one deliberate
/// exception to doctor's no-spawn rule, mirroring exactly what serve/chat do
/// at startup.
fn sandbox_check() -> Check {
    let (kind, sandboxed) = octos_agent::sandbox::auto_sandbox_kind();
    if sandboxed {
        Check::pass(CAT_SANDBOX, "sandbox backend", format!("Auto → {kind}"))
    } else {
        Check::warn(
            CAT_SANDBOX,
            "sandbox backend",
            format!("Auto → {kind}"),
            "install a backend (macOS: sandbox-exec · Linux: bubblewrap · any: Docker)",
        )
    }
}

/// Installed skills: count manifests under `<data_dir>/skills/*/manifest.json`
/// and flag unparseable ones. Read-only — no gating probes, no spawning.
fn skills_check(data_dir: &Path) -> Check {
    let dir = data_dir.join("skills");
    if !dir.exists() {
        return Check::pass(CAT_SKILLS, "skills", "none installed");
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // The dir EXISTS but can't be listed — say so instead of a
        // false-green "none installed" (codex).
        Err(error) => {
            return Check::warn(
                CAT_SKILLS,
                "skills",
                format!("skills dir exists but cannot be listed: {error}"),
                "check the directory's permissions/ownership",
            );
        }
    };
    let mut ok = 0usize;
    let mut broken: Vec<String> = Vec::new();
    // `take` before `flatten`: bound RAW entries, not just successful ones.
    for entry in entries.take(MAX_SCAN_ENTRIES).flatten() {
        let manifest = entry.path().join("manifest.json");
        if !manifest.is_file() {
            continue;
        }
        let oversized = manifest
            .metadata()
            .map(|meta| meta.len() > MAX_JSON_READ_BYTES)
            .unwrap_or(false);
        let parsed = (!oversized)
            .then(|| std::fs::read_to_string(&manifest).ok())
            .flatten()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        if parsed.is_some() {
            ok += 1;
        } else {
            broken.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    if broken.is_empty() {
        Check::pass(CAT_SKILLS, "skills", format!("{ok} installed"))
    } else {
        // Report the TRUE broken count; truncate only the name list.
        let total_broken = broken.len();
        broken.sort();
        broken.truncate(3);
        let shown = broken.join(", ");
        let suffix = if total_broken > 3 { ", …" } else { "" };
        Check::warn(
            CAT_SKILLS,
            "skills",
            format!("{ok} ok, {total_broken} with unreadable manifest(s): {shown}{suffix}"),
            "reinstall the broken skill(s): `octos skills remove/install <name>`",
        )
    }
}

/// Configured MCP servers: stdio commands must resolve on PATH (nothing is
/// spawned); HTTP servers are listed with their URL (no probe — they are
/// often internal).
fn mcp_checks(config: &crate::config::Config) -> Vec<Check> {
    let servers = &config.mcp_servers;
    if servers.is_empty() {
        return vec![Check::pass(CAT_MCP, "MCP servers", "none configured")];
    }
    let dirs = path_dirs();
    let mut checks = vec![Check::pass(
        CAT_MCP,
        "MCP servers",
        format!("{} configured", servers.len()),
    )];
    for (index, server) in servers.iter().take(MAX_PROFILE_ROWS).enumerate() {
        // Transport precedence mirrors the runtime: `url` wins whenever it is
        // set (`McpServerConfig` dispatch), so a server with BOTH gets its
        // HTTP row, not a stdio false-green (codex r2).
        match (&server.url, &server.command) {
            (Some(url), _) => checks.push(
                // Display-sanitized: an MCP URL may carry a token in its
                // userinfo/query, which must not land in the support bundle.
                Check::pass(CAT_MCP, format!("mcp[{index}]"), "HTTP transport")
                    .with_value(sanitize_url_for_display(url)),
            ),
            (None, Some(command)) => {
                // A configured command may be an absolute path or a bare
                // name. PATH resolution only — executability is not probed.
                let resolvable = Path::new(command).is_file() || binary_on_path(&dirs, command);
                if resolvable {
                    checks.push(Check::pass(
                        CAT_MCP,
                        format!("mcp[{index}] {command}"),
                        "stdio command resolvable",
                    ));
                } else {
                    checks.push(Check::warn(
                        CAT_MCP,
                        format!("mcp[{index}] {command}"),
                        "stdio command not found on PATH",
                        "install it or fix mcp_servers[].command in config.json",
                    ));
                }
            }
            (None, None) => checks.push(Check::warn(
                CAT_MCP,
                format!("mcp[{index}]"),
                "neither command nor url configured",
                "set mcp_servers[].command (stdio) or .url (HTTP) in config.json",
            )),
        }
    }
    checks
}

/// Gateway channel inventory (informational — token/secret checks stay out of
/// doctor to avoid guessing per-channel auth schemes).
fn channels_check(config: &crate::config::Config) -> Check {
    let email = config.email.is_some();
    match &config.gateway {
        None => Check::pass(
            CAT_CHANNELS,
            "gateway channels",
            if email {
                "gateway not configured (email channel configured)".to_string()
            } else {
                "gateway not configured — chat/serve modes only".to_string()
            },
        ),
        Some(gateway) => {
            let mut types: Vec<&str> = gateway
                .channels
                .iter()
                .map(|entry| entry.channel_type.as_str())
                .collect();
            types.sort_unstable();
            types.dedup();
            let email_note = if email { " · email configured" } else { "" };
            Check::pass(
                CAT_CHANNELS,
                "gateway channels",
                format!(
                    "{} channel(s): {}{email_note}",
                    gateway.channels.len(),
                    if types.is_empty() {
                        "none".to_string()
                    } else {
                        types.join(", ")
                    }
                ),
            )
        }
    }
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
    fn install_method_for_path_infers_from_path() {
        assert_eq!(
            install_method_for_path(Path::new("/home/u/.cargo/bin/octos")),
            "cargo"
        );
        assert_eq!(
            install_method_for_path(Path::new("/opt/homebrew/bin/octos")),
            "brew"
        );
        assert_eq!(
            install_method_for_path(Path::new("/home/u/.local/bin/octoscode")),
            "shell installer"
        );
        assert_eq!(
            install_method_for_path(Path::new("/home/u/.octos/bin/octos")),
            "octoscode auto-install"
        );
        assert_eq!(
            install_method_for_path(Path::new("/usr/bin/octos")),
            "system"
        );
        assert_eq!(
            install_method_for_path(Path::new("/x/node_modules/.bin/octoscode")),
            "npm"
        );
    }

    #[test]
    fn installs_check_passes_on_one_and_warns_on_duplicates() {
        let one = installs_check(
            "octos",
            &LocatedBinaries {
                on_path: vec![PathBuf::from("/opt/homebrew/bin/octos")],
                off_path: vec![],
            },
        );
        assert_eq!(one.status, CheckStatus::Pass);

        let dup = installs_check(
            "octos",
            &LocatedBinaries {
                on_path: vec![PathBuf::from("/opt/homebrew/bin/octos")],
                off_path: vec![PathBuf::from("/home/u/.cargo/bin/octos")],
            },
        );
        assert_eq!(dup.status, CheckStatus::Warn);

        assert_eq!(
            installs_check("octos", &LocatedBinaries::default()).status,
            CheckStatus::Warn
        );
    }

    #[test]
    fn installations_checks_cover_both_octos_and_octoscode() {
        let checks = installations_checks(&octos_server_spec());
        assert!(checks.iter().any(|c| c.name == "octos installs"));
        assert!(checks.iter().any(|c| c.name == "octoscode installs"));
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
        // The server's own build advertises every default feature (v2 is
        // intentionally strict opt-in), so the structural skew check passes.
        let skew = report
            .checks
            .iter()
            .find(|c| c.name == "protocol skew")
            .expect("protocol skew check present");
        assert_eq!(skew.status, octos_diagnostics::CheckStatus::Pass);
        // Glyphs are present in the rendered output.
        assert!(text.contains("[✓]"));
    }

    // ---- Stage 3 ----

    /// Hermetic context rooted in a temp dir so no real ~/.octos is touched.
    fn temp_ctx(temp: &tempfile::TempDir) -> crate::config_context::ConfigContext {
        crate::config_context::ConfigContext {
            config_home: temp.path().join("config-home"),
            auth_home: temp.path().join("auth-home"),
            data_dir: temp.path().join("data"),
            is_default: false,
        }
    }

    #[test]
    fn should_fail_config_check_with_exact_error_when_config_json_is_broken() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = temp_ctx(&temp);
        std::fs::create_dir_all(&ctx.config_home).unwrap();
        let path = ctx.config_home.join("config.json");
        std::fs::write(&path, "{ \"provider\": ").unwrap(); // truncated JSON
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let resolved = crate::config::resolve_config_file_path(&cwd, &ctx, None);
        let (check, config) = config_parse_check(&resolved, &cwd, &ctx);
        assert!(
            config.is_none(),
            "broken config must not yield an effective config"
        );
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(
            check.fix.as_deref().unwrap_or("").contains("config"),
            "fix must point at the config file"
        );
    }

    #[test]
    fn should_pass_config_check_with_defaults_note_when_no_file() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = temp_ctx(&temp);
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let resolved = crate::config::resolve_config_file_path(&cwd, &ctx, None);
        let (check, config) = config_parse_check(&resolved, &cwd, &ctx);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("defaults"));
        assert!(config.is_some(), "defaults are a usable effective config");
    }

    #[test]
    fn should_fail_api_key_check_when_unresolvable_and_pass_for_keyless_provider() {
        // An UNREGISTERED provider with an unresolvable key: WARN, not FAIL —
        // unknown/custom providers may legitimately be keyless (codex r2).
        let mut config = crate::config::Config {
            provider: Some("doctortestonly".into()),
            ..Default::default()
        };
        let checks = provider_checks(&config, false);
        let key = checks
            .iter()
            .find(|c| c.name == "API key")
            .expect("key check present");
        assert_eq!(key.status, CheckStatus::Warn);
        assert_eq!(key.value.as_deref(), Some("DOCTORTESTONLY_API_KEY"));

        // A REGISTERED cloud provider (key env set, no base_url requirement)
        // with an unresolvable key is a hard FAIL — octos won't run a turn.
        // `zzz_doctor_env` guards against a real key in the test env.
        let cloud = crate::config::Config {
            provider: Some("anthropic".into()),
            api_key_env: Some("ZZZ_DOCTOR_TEST_UNSET_ENV".into()),
            ..Default::default()
        };
        let checks = provider_checks(&cloud, false);
        let key = checks
            .iter()
            .find(|c| c.name == "API key")
            .expect("key check present");
        assert_eq!(key.status, CheckStatus::Fail);

        // env_vars-supplied key resolves through the real chain (redacted).
        config.api_key_env = Some("DOCTORTESTONLY2_API_KEY".into());
        config
            .env_vars
            .insert("DOCTORTESTONLY2_API_KEY".into(), "sekrit-value".into());
        let checks = provider_checks(&config, false);
        let key = checks
            .iter()
            .find(|c| c.name == "API key")
            .expect("key check present");
        assert_eq!(key.status, CheckStatus::Pass);
        assert!(
            !format!("{:?}", key).contains("sekrit-value"),
            "the key value must never appear in the check"
        );

        // Local providers need no key at all.
        let config = crate::config::Config {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        let checks = provider_checks(&config, false);
        let key = checks
            .iter()
            .find(|c| c.name == "API key")
            .expect("key check present");
        assert_eq!(key.status, CheckStatus::Pass);
    }

    #[test]
    fn should_warn_provider_check_when_no_provider_configured() {
        let checks = provider_checks(&crate::config::Config::default(), false);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0].detail.contains("no LLM provider"));
    }

    #[test]
    fn should_detect_redb_store_held_by_another_process() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("episodes.redb");
        let held = redb::Database::create(&path).expect("create store");

        // While held: the single-writer lock classifies as in-use (warn).
        let check = store_lock_check(CAT_STORES, "episodes (t)".into(), &path)
            .expect("existing file yields a check");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("another octos process"));

        // Released: openable again.
        drop(held);
        let check = store_lock_check(CAT_STORES, "episodes (t)".into(), &path)
            .expect("existing file yields a check");
        assert_eq!(check.status, CheckStatus::Pass);

        // Absent file yields no row at all.
        assert!(
            store_lock_check(CAT_STORES, "x".into(), &temp.path().join("missing.redb")).is_none()
        );
    }

    #[test]
    fn should_sum_disk_usage_and_format_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("a.jsonl"), vec![b'x'; 1500]).unwrap();
        std::fs::write(dir.join("nested/b.jsonl"), vec![b'y'; 500]).unwrap();
        let usage = disk_usage(&[dir]);
        assert_eq!(usage.bytes, 2000);
        assert_eq!(usage.files, 2);
        assert!(!usage.capped);
        assert!(!usage.unreadable);
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KiB");
    }

    #[test]
    fn should_resolve_binaries_only_from_injected_path_dirs() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("bwrap"), "").unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        assert!(binary_on_path(&dirs, "bwrap"));
        assert!(!binary_on_path(&dirs, "definitely-not-here-xyz"));
    }

    #[test]
    fn should_flag_profiles_without_llm_selection() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        let profiles_dir = data_dir.join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            profiles_dir.join("glm.json"),
            format!(
                r#"{{"id":"glm","name":"glm","enabled":true,"config":{{}},"created_at":"{now}","updated_at":"{now}"}}"#
            ),
        )
        .unwrap();
        std::fs::write(profiles_dir.join("broken.json"), "not json").unwrap();

        let (profiles, dir_error) = load_profiles(&data_dir);
        assert!(dir_error.is_none());
        assert_eq!(profiles.len(), 2);
        let checks = profile_checks(&profiles);
        let broken = checks
            .iter()
            .find(|c| c.name == "profile broken")
            .expect("broken profile row");
        assert_eq!(broken.status, CheckStatus::Warn);
        let glm = checks
            .iter()
            .find(|c| c.name == "profile glm")
            .expect("glm row");
        assert_eq!(glm.status, CheckStatus::Warn);
        assert!(glm.detail.contains("no LLM selection"));
    }

    #[test]
    fn should_count_skills_and_flag_broken_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(data_dir.join("skills/good")).unwrap();
        std::fs::create_dir_all(data_dir.join("skills/bad")).unwrap();
        std::fs::write(data_dir.join("skills/good/manifest.json"), "{}").unwrap();
        std::fs::write(data_dir.join("skills/bad/manifest.json"), "nope").unwrap();
        let check = skills_check(&data_dir);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("bad"));

        // No skills dir at all is a clean pass.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(skills_check(empty.path()).status, CheckStatus::Pass);
    }

    fn mcp_server(command: Option<&str>, url: Option<&str>) -> octos_agent::McpServerConfig {
        octos_agent::McpServerConfig {
            command: command.map(String::from),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            url: url.map(String::from),
            headers: std::collections::HashMap::new(),
            oauth: false,
            scopes: Vec::new(),
            concurrency_class: None,
        }
    }

    #[test]
    fn should_check_mcp_stdio_commands_and_list_http_servers() {
        let config = crate::config::Config {
            mcp_servers: vec![
                mcp_server(Some("definitely-not-installed-xyz"), None),
                mcp_server(None, Some("http://127.0.0.1:9999/mcp")),
            ],
            ..Default::default()
        };
        let checks = mcp_checks(&config);
        assert!(checks[0].detail.contains("2 configured"));
        assert_eq!(
            checks[1].status,
            CheckStatus::Warn,
            "missing stdio command warns"
        );
        assert_eq!(checks[2].status, CheckStatus::Pass, "http server listed");
        assert_eq!(
            checks[2].value.as_deref(),
            Some("http://127.0.0.1:9999/mcp")
        );

        let none = mcp_checks(&crate::config::Config::default());
        assert_eq!(none.len(), 1);
        assert!(none[0].detail.contains("none configured"));
    }

    #[test]
    fn should_redact_quoted_literals_from_error_text() {
        // Serde type errors quote the offending input — a secret in the wrong
        // config field must not survive into the report / support bundle.
        let error =
            r#"invalid type: string "sk-super-secret-key", expected a map at line 3 column 20"#;
        let redacted = redact_quoted_literals(error);
        assert!(!redacted.contains("sk-super-secret-key"));
        assert!(redacted.contains("[redacted]"));
        assert!(
            redacted.contains("line 3 column 20"),
            "location info must survive: {redacted}"
        );

        // Escaped quotes INSIDE the literal (serde formats strings with
        // `{:?}`) must not close the segment early and leak the remainder.
        let tricky = r#"invalid value: string "prefix \"sk-inner-secret\" tail", expected x"#;
        let redacted = redact_quoted_literals(tricky);
        assert!(
            !redacted.contains("sk-inner-secret"),
            "escaped-quote content leaked: {redacted}"
        );

        // Unterminated literal at end-of-message: everything after the
        // opening quote is dropped, no panic.
        let unterminated = r#"error near "sk-trailing-secret"#;
        let redacted = redact_quoted_literals(unterminated);
        assert!(!redacted.contains("sk-trailing-secret"));
    }

    #[test]
    fn should_reject_non_http_urls_and_guard_env_names() {
        // Opaque schemes carry their payload verbatim — never display them.
        assert_eq!(
            sanitize_url_for_display("data:text/plain,sk-opaque-secret"),
            "(non-HTTP or unparseable URL)"
        );
        assert!(probe_endpoint("data:text/plain,sk-opaque-secret").is_err());
        assert!(probe_endpoint("file:///etc/passwd").is_err());

        // A key pasted where the env var NAME belongs must not be echoed.
        assert_eq!(
            display_env_name("sk-live-abc123.secret".into()),
            "[api_key_env is not a valid env var name — redacted]"
        );
        assert_eq!(display_env_name("ZAI_API_KEY".into()), "ZAI_API_KEY");

        // …including through the RESOLVER's error text, which echoes the env
        // var name it looked up (live-caught residual): no check field may
        // carry the pasted value.
        let config = crate::config::Config {
            provider: Some("anthropic".into()),
            api_key_env: Some("sk-live-PASTED-999".into()),
            ..Default::default()
        };
        let checks = provider_checks(&config, false);
        for check in &checks {
            let rendered = format!("{check:?}");
            assert!(
                !rendered.contains("sk-live-PASTED-999"),
                "pasted secret leaked through check: {rendered}"
            );
        }
    }

    #[test]
    fn should_strip_credentials_and_query_from_displayed_and_probed_urls() {
        assert_eq!(
            sanitize_url_for_display("https://user:hunter2@api.example.com/v1?api_key=tok#frag"),
            "https://api.example.com/v1"
        );
        assert_eq!(
            sanitize_url_for_display("not a url"),
            "(non-HTTP or unparseable URL)"
        );
        // The probe itself rejects unparseable URLs instead of sending them.
        assert!(probe_endpoint("not a url").is_err());
    }

    // ---- Stage 4: session inventory ----

    fn write_session(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn should_inventory_sessions_across_main_and_user_stores() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        write_session(
            &data_dir.join("sessions"),
            "glm%3Alocal%3Atui%23coding.jsonl",
            &[
                r#"{"role":"user","content":"x"}"#,
                r#"{"role":"assistant"}"#,
            ],
        );
        write_session(
            &data_dir.join("users/u1/sessions"),
            "topic.jsonl",
            &[r#"{"role":"user"}"#],
        );

        let checks = session_checks(&data_dir, &[]);
        assert_eq!(checks.len(), 1, "one row for the server store");
        let row = &checks[0];
        assert_eq!(row.status, CheckStatus::Pass);
        assert!(row.detail.contains("2 session(s)"), "{}", row.detail);
        assert!(row.detail.contains("ago"), "ages present: {}", row.detail);
    }

    #[test]
    fn should_flag_corrupt_tail_but_not_valid_or_empty_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let dir = data_dir.join("sessions");
        write_session(&dir, "good.jsonl", &[r#"{"role":"user"}"#]);
        write_session(
            &dir,
            "truncated.jsonl",
            &[r#"{"role":"user"}"#, r#"{"role":"assis"#],
        );
        std::fs::write(dir.join("empty.jsonl"), "").unwrap();

        let checks = session_checks(&data_dir, &[]);
        let row = &checks[0];
        assert_eq!(row.status, CheckStatus::Warn);
        assert!(
            row.detail.contains("1 with unparseable tail: truncated"),
            "{}",
            row.detail
        );
        assert!(!row.detail.contains("good,"), "valid session not flagged");
        assert!(
            row.fix.as_deref().unwrap_or("").contains("resume"),
            "fix explains the consequence"
        );
    }

    #[test]
    fn should_pass_when_a_large_transcript_has_a_valid_tail_beyond_probe_window() {
        // A file bigger than the 64 KiB probe window whose tail is healthy
        // must NOT be flagged (the window cut lands mid-line at the START of
        // the window, never the end).
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let line = format!(r#"{{"role":"user","content":"{}"}}"#, "x".repeat(200));
        let lines: Vec<&str> = std::iter::repeat_n(line.as_str(), 600).collect();
        write_session(&data_dir.join("sessions"), "big.jsonl", &lines);
        let meta = std::fs::metadata(data_dir.join("sessions/big.jsonl")).unwrap();
        assert!(
            meta.len() > SESSION_TAIL_PROBE_BYTES,
            "test file must exceed the window"
        );

        let checks = session_checks(&data_dir, &[]);
        assert_eq!(checks[0].status, CheckStatus::Pass, "{}", checks[0].detail);
    }

    #[test]
    fn should_flag_sessions_near_the_write_cap() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let dir = data_dir.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        // Sparse 9 MiB file with a valid JSON tail: metadata length is what
        // counts (same trick as the disk-usage soak).
        let path = dir.join("huge.jsonl");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(9 * 1024 * 1024).unwrap();
        drop(file);
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(b"{\"role\":\"user\"}\n").unwrap();
        }

        let checks = session_checks(&data_dir, &[]);
        let row = &checks[0];
        assert_eq!(row.status, CheckStatus::Warn);
        assert!(
            row.detail.contains("1 near the 10 MiB session cap"),
            "{}",
            row.detail
        );
    }

    #[test]
    fn should_not_flag_a_single_giant_line_as_corrupt() {
        // A final record longer than the probe window arrives as a FRAGMENT
        // (no newline in the window) — verdict must be UNKNOWN, not corrupt:
        // flagging it would advise deleting a healthy session (codex).
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let giant = format!(
            r#"{{"role":"user","content":"{}"}}"#,
            "y".repeat((SESSION_TAIL_PROBE_BYTES as usize) + 4096)
        );
        std::fs::write(dir.join("giant.jsonl"), &giant).unwrap();

        let checks = session_checks(temp.path(), &[]);
        assert_eq!(
            checks[0].status,
            CheckStatus::Pass,
            "giant single-line session must not be flagged: {}",
            checks[0].detail
        );

        // But a SMALL file with no newline and invalid JSON is definitively
        // corrupt (the whole file is in the window: start == 0).
        std::fs::write(dir.join("smallbad.jsonl"), "not json at all").unwrap();
        let checks = session_checks(temp.path(), &[]);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(
            checks[0].detail.contains("smallbad"),
            "{}",
            checks[0].detail
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_skip_symlinked_session_files() {
        // A symlink dropped into the store must not be read (it points
        // outside the store boundary — same no-follow rule as the disk walk).
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(temp.path().join("outside.jsonl"), "not json").unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside.jsonl"), dir.join("linked.jsonl"))
            .unwrap();
        std::fs::write(dir.join("real.jsonl"), "{}").unwrap();

        let checks = session_checks(temp.path(), &[]);
        assert_eq!(checks[0].status, CheckStatus::Pass, "{}", checks[0].detail);
        assert!(
            checks[0].detail.contains("1 session(s)"),
            "symlink not counted: {}",
            checks[0].detail
        );
    }

    #[test]
    fn should_dedup_profile_stores_pointing_at_the_server_root() {
        // A profile whose data_dir override IS the data-dir root must not
        // produce a second inventory of the same store.
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        write_session(&data_dir.join("sessions"), "s.jsonl", &[r#"{"a":1}"#]);
        let now = chrono::Utc::now().to_rfc3339();
        // Build via `json!` so a Windows `data_dir` (backslashes) is escaped
        // correctly. Interpolating it into a raw JSON string produced invalid
        // escapes (`\U`, `\A`, ...) that made `from_str` panic on Windows.
        let profile: crate::profiles::UserProfile = serde_json::from_value(serde_json::json!({
            "id": "alias",
            "name": "alias",
            "enabled": true,
            "config": {},
            "data_dir": data_dir.to_string_lossy(),
            "created_at": now.clone(),
            "updated_at": now,
        }))
        .unwrap();
        let profiles: Vec<DiscoveredProfile> = vec![("alias".into(), Ok(profile))];

        let checks = session_checks(&data_dir, &profiles);
        let rows = checks
            .iter()
            .filter(|c| c.name.starts_with("sessions ("))
            .count();
        assert_eq!(rows, 1, "duplicate store must be deduped: {checks:?}");
    }

    #[test]
    fn should_skip_task_ledger_sidecars() {
        // `<key>.tasks.jsonl` sidecars are task ledgers, not transcripts —
        // they must not count, consume budget, or trigger resume/fork advice
        // (session keys are percent-encoded, so a dotted stem = sidecar).
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        write_session(&dir, "glm%3Alocal.jsonl", &[r#"{"role":"user"}"#]);
        std::fs::write(dir.join("glm%3Alocal.tasks.jsonl"), "not json").unwrap();

        let checks = session_checks(temp.path(), &[]);
        assert_eq!(checks[0].status, CheckStatus::Pass, "{}", checks[0].detail);
        assert!(
            checks[0].detail.contains("1 session(s)"),
            "sidecar not counted: {}",
            checks[0].detail
        );
    }

    #[test]
    fn should_report_none_stored_when_no_sessions_exist_anywhere() {
        let temp = tempfile::tempdir().unwrap();
        let checks = session_checks(temp.path(), &[]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Pass);
        assert!(checks[0].detail.contains("none stored yet"));
    }

    #[test]
    fn should_inventory_profile_session_stores_by_their_base() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let base = data_dir.join("profiles/glm/data");
        write_session(&base.join("sessions"), "s.jsonl", &[r#"{"role":"user"}"#]);
        let now = chrono::Utc::now().to_rfc3339();
        let profile: crate::profiles::UserProfile = serde_json::from_str(&format!(
            r#"{{"id":"glm","name":"glm","enabled":true,"config":{{}},"created_at":"{now}","updated_at":"{now}"}}"#
        ))
        .unwrap();
        let profiles: Vec<DiscoveredProfile> = vec![("glm".into(), Ok(profile))];

        let checks = session_checks(&data_dir, &profiles);
        assert!(
            checks
                .iter()
                .any(|c| c.name == "sessions (glm)" && c.detail.contains("1 session(s)")),
            "profile store row present"
        );
    }

    #[test]
    fn should_render_notes_block_only_when_something_is_flagged() {
        let all_pass = Report::new(vec![Check::pass("C", "fine", "ok")]);
        assert!(render_notes(&all_pass).is_empty());

        let flagged = Report::new(vec![
            Check::pass("C", "fine", "ok"),
            Check::warn("C", "worrying", "something is off", "do the thing"),
        ]);
        let notes = render_notes(&flagged);
        assert!(notes.starts_with("Notes\n"));
        assert!(notes.contains("worrying"));
        assert!(
            !notes.contains("fine — ok"),
            "passing rows stay out of notes"
        );
    }
}
