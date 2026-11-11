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
//!   total size, newest/oldest age, and transcripts whose final line no
//!   longer parses (the crash-mid-write signature that breaks resume). The
//!   tail probe parses and immediately discards; no transcript content
//!   reaches the report.
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
    UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1, UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2,
    UI_PROTOCOL_KNOWN_FEATURES, UI_PROTOCOL_V1, UiProtocolCapabilities,
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

    // --- Embedded web UI bundles --------------------------------------------
    // What this binary can actually serve at /app, /admin, and /swarm. The
    // SPAs are embedded from gitignored static/ trees, so a source-clone
    // build without the scripts/build-*.sh step 503s at every UI route
    // (#2384) — surface that before the first serve.
    #[cfg(feature = "api")]
    report.push(ui_bundles_check());

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
        report.push(embedder_check(&ctx.data_dir));
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
    // no-header compatibility baseline. `projection.envelope.v2` and
    // `context.semantic_cache.v1` are known but strictly opt-in (advertised
    // only after explicit client negotiation, UPCR-2026-029), so exclude them
    // here; otherwise doctor would mistake the deliberately absent default
    // advertisement for protocol skew.
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
            .filter(|feature| *feature != UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2)
            .filter(|feature| *feature != UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1)
            // `context.state.v1` is the same kind of strict opt-in: it adds
            // a notification legacy clients cannot decode, so the baseline
            // never claims it.
            .filter(|feature| {
                *feature != octos_core::ui_protocol::UI_PROTOCOL_FEATURE_CONTEXT_STATE_V1
            })
            // Full-history retry is acknowledged only when requested, too.
            // Its absence from the no-header baseline is not protocol skew.
            .filter(|feature| {
                *feature
                    != octos_core::ui_protocol::UI_PROTOCOL_FEATURE_SESSION_APPEND_FULL_HISTORY_RETRY_V1
            })
            // Same for the participant field (UPCR-2026-053).
            .filter(|feature| {
                *feature
                    != octos_core::ui_protocol::UI_PROTOCOL_FEATURE_SESSION_APPEND_PARTICIPANT_V1
            })
            // And for the workspace peer message record (UPCR-2026-RF506E103).
            .filter(|feature| {
                *feature
                    != octos_core::ui_protocol::UI_PROTOCOL_FEATURE_PEER_TEAM_MESSAGE_RECORD_V1
            }),
    ));

    Ok(report)
}

/// What this binary can serve at the embedded UI routes (#2384): pass with
/// the available bundles, warn naming the missing ones' build scripts.
#[cfg(feature = "api")]
fn ui_bundles_check() -> Check {
    ui_bundles_check_from(
        &crate::api::static_files::embedded_ui_bundles(),
        cfg!(debug_assertions),
    )
}

#[cfg(feature = "api")]
fn ui_bundles_check_from(
    bundles: &[crate::api::static_files::UiBundleStatus],
    reads_static_from_disk: bool,
) -> Check {
    let missing: Vec<&crate::api::static_files::UiBundleStatus> =
        bundles.iter().filter(|bundle| !bundle.embedded).collect();
    if missing.is_empty() {
        let present: Vec<&str> = bundles
            .iter()
            .filter(|bundle| bundle.embedded)
            .map(|bundle| bundle.label)
            .collect();
        return Check::pass(CAT_BINARY, "web UI bundles", present.join(", "))
            .with_value(embed_mode_value(reads_static_from_disk));
    }
    let scripts = missing
        .iter()
        .map(|bundle| bundle.build_script)
        .collect::<Vec<_>>()
        .join(", ");
    let rebuild = if reads_static_from_disk {
        // A debug binary reads `static/` from disk at serve time — the
        // scripts' output is picked up without recompiling.
        " (debug build reads static/ from disk; no rebuild needed)"
    } else {
        ", then rebuild octos-cli"
    };
    Check::warn(
        CAT_BINARY,
        "web UI bundles",
        format!(
            "not available: {} — those UI routes serve 503",
            missing
                .iter()
                .map(|bundle| bundle.label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!("run {scripts}{rebuild}"),
    )
    .with_value(embed_mode_value(reads_static_from_disk))
}

#[cfg(feature = "api")]
fn embed_mode_value(reads_static_from_disk: bool) -> &'static str {
    if reads_static_from_disk {
        "debug build: static/ read from disk at runtime"
    } else {
        "release build: bundles compiled in"
    }
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
        // Keyless family — but the runtime still opportunistically resolves
        // the derived `{NAME}_API_KEY` chain and sends whatever it finds as a
        // Bearer token (chat.rs provider factory). An ambient credential going
        // out silently while doctor says "no key" is exactly the kind of
        // contradiction doctor exists to surface — disclose it.
        match config.get_api_key_with_env(canonical, None) {
            Ok(_) => checks.push(
                Check::pass(
                    CAT_PROVIDER,
                    "API key",
                    format!(
                        "{canonical} needs no API key, but a credential resolves via {env_name} \
                         and WILL be sent as a Bearer token"
                    ),
                )
                .with_value(env_name.clone()),
            ),
            Err(_) => checks.push(Check::pass(
                CAT_PROVIDER,
                "API key",
                format!("{canonical} is a local provider — no API key required"),
            )),
        }
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
        match endpoint.as_deref() {
            Some(url) => {
                let display = endpoint_display.unwrap_or_else(|| "(unparseable URL)".into());
                match probe_endpoint(url) {
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

        // Local model servers all answer the OpenAI `GET /v1/models` shape —
        // one request verifies the server AND surfaces the loaded model ids.
        if is_local_server_family(canonical) {
            // Authenticate exactly as the runtime would (auth store → env
            // chain): a `llama-server --api-key` deployment must list models
            // instead of false-alarming with 401 (codex + adversarial pass).
            let api_key = config
                .get_api_key_with_env(canonical, config.api_key_env.as_deref())
                .ok();
            checks.extend(local_server_checks(
                canonical,
                endpoint.as_deref(),
                config.model.as_deref(),
                api_key.as_deref(),
            ));
        }
    }

    checks
}

/// Families that are a local OpenAI-compatible server (unified `local` plus
/// the engine-branded `ollama`/`vllm`), where `/models` discovery applies.
fn is_local_server_family(canonical: &str) -> bool {
    matches!(canonical, "local" | "ollama" | "vllm")
}

/// `/models` discovery for a local server: reachability with a model list on
/// success, and a cross-check that the configured model is actually loaded
/// (on Ollama-style servers the `model` field selects the model, so a
/// mismatch means every request would 404).
fn local_server_checks(
    canonical: &str,
    endpoint: Option<&str>,
    configured_model: Option<&str>,
    api_key: Option<&str>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(base) = endpoint else {
        return checks;
    };
    match probe_models(base, api_key) {
        Ok(Some(models)) if !models.is_empty() => {
            const SHOWN: usize = 5;
            let mut shown: Vec<String> =
                models.iter().take(SHOWN).map(|id| sanitize_display_id(id)).collect();
            let rest = models.len().saturating_sub(SHOWN);
            if rest > 0 {
                shown.push(format!("… +{rest} more"));
            }
            checks.push(
                Check::pass(
                    CAT_PROVIDER,
                    "local models",
                    format!("server lists {} model(s)", models.len()),
                )
                .with_value(shown.join(", ")),
            );
            // The unified family's placeholder stands for "single-model server
            // that ignores the field" — never flag it.
            if let Some(model) = configured_model
                .filter(|m| *m != octos_llm::local_discovery::PLACEHOLDER_MODEL)
            {
                let loaded = models.iter().any(|id| local_model_matches(model, id));
                if !loaded {
                    checks.push(Check::warn(
                        CAT_PROVIDER,
                        "local model configured",
                        format!("\"{model}\" is not among the server's listed models"),
                        "set \"model\" to one of the listed ids (or leave it unset for single-model servers)",
                    ));
                }
            }
        }
        Ok(Some(_)) => checks.push(Check::warn(
            CAT_PROVIDER,
            "local models",
            "server answered /models but listed none",
            "load a model first (llama.cpp: `llama-server -m model.gguf`; ollama: `ollama pull <model>`)",
        )),
        // Answered 2xx, but not the OpenAI list-models shape: most likely an
        // unrelated app owns the port — "load a model" would be wrong advice.
        Ok(None) => checks.push(Check::warn(
            CAT_PROVIDER,
            "local models",
            "the endpoint answered, but not with an OpenAI-compatible model list — is something else running on this port?",
            "point \"base_url\" at the model server's /v1 endpoint (llama.cpp default: http://127.0.0.1:8080/v1)",
        )),
        Err(error) => {
            // A 401/403 means the server IS there and wants its key — "is the
            // server running?" would be false advice (codex P2).
            let auth_rejected = error.contains("HTTP 401") || error.contains("HTTP 403");
            checks.push(if auth_rejected {
                Check::warn(
                    CAT_PROVIDER,
                    "local models",
                    format!(
                        "server rejected the request as unauthorized ({error}) — model listing skipped"
                    ),
                    if api_key.is_some() {
                        "the resolved API key was rejected — check it matches the server's --api-key"
                    } else {
                        "the server requires its API key — set \"api_key_env\" to the env var holding it"
                    },
                )
            } else {
                Check::warn(
                    CAT_PROVIDER,
                    "local models",
                    format!("could not list models: {error}"),
                    format!(
                        "is the server running? common local endpoints: {}",
                        octos_llm::local_discovery::CANDIDATE_BASE_URLS.join(", ")
                    ),
                )
            });
        }
    }
    // Agent use depends on tool calling, which local servers only provide
    // with a tool-capable model + chat template — a connect-fine/tools-broken
    // setup otherwise looks like an octos bug. Named "(advisory)" because
    // nothing is verified here; a bare pass would read as a checked result.
    if canonical == "local" {
        checks.push(Check::pass(
            CAT_PROVIDER,
            "tool calling (advisory)",
            "agent tools need a tool-capable model and chat template (llama.cpp: start llama-server with --jinja)",
        ));
    }
    checks
}

/// Whether a configured model name refers to a server-listed id, with
/// Ollama's tag semantics: comparison is case-insensitive, and a bare `name`
/// is equivalent to `name:latest` — in BOTH directions, and ONLY for the
/// `latest` tag. A bare `qwen2.5` does NOT match `qwen2.5:7b`: Ollama would
/// resolve it to the absent `qwen2.5:latest`, so the request would fail and
/// the warning is correct (codex P2 + adversarial pass, opposite directions).
fn local_model_matches(configured: &str, listed: &str) -> bool {
    fn canon(id: &str) -> String {
        let lower = id.to_lowercase();
        lower
            .strip_suffix(":latest")
            .map(str::to_owned)
            .unwrap_or(lower)
    }
    canon(configured) == canon(listed)
}

/// One server-supplied string, made safe for terminal display: control
/// characters (ANSI/OSC escapes included) become U+FFFD and the id is capped,
/// so a squatted port cannot rewrite doctor's report (multi-model finding).
/// Mirrors the `display_env_name` guard for the other untrusted-value path.
fn sanitize_display_id(id: &str) -> String {
    const MAX_ID_CHARS: usize = 96;
    let mut out: String = id
        .chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .take(MAX_ID_CHARS)
        .collect();
    if id.chars().count() > MAX_ID_CHARS {
        out.push('…');
    }
    out
}

/// Shared short-timeout, no-redirect blocking client for doctor's endpoint
/// probes — one place for the hardening rules so `probe_endpoint` and
/// `probe_models` cannot drift (maintainability pass).
fn probe_client() -> std::result::Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        // No redirect following: a redirect target could carry credentials
        // that would then be echoed through the error path (codex r2).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
}

/// GET `{base}/models` (same credential-stripping and no-redirect rules as
/// [`probe_endpoint`], via [`probe_client`]) and parse the OpenAI list-models
/// body. The path segment is appended AFTER URL parsing so a base_url carrying
/// a query/fragment cannot silently redirect the probe to the wrong path.
/// Returns Ok(None) when the body is not the list-models shape. Body reads
/// are capped; a body exceeding the cap is an explicit error, not a silent
/// mis-parse of truncated JSON.
fn probe_models(
    base: &str,
    api_key: Option<&str>,
) -> std::result::Result<Option<Vec<String>>, String> {
    use std::io::Read as _;
    const MAX_BODY_BYTES: u64 = 256 * 1024;
    let mut url = sanitized_http_url(base)?;
    url.path_segments_mut()
        .map_err(|_| "base_url cannot take a path".to_string())?
        .pop_if_empty()
        .push("models");
    let client = probe_client()?;
    let mut request = client.get(url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let mut body = String::new();
    response
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(format!(
            "response larger than {}KB — not a model list",
            MAX_BODY_BYTES / 1024
        ));
    }
    Ok(octos_llm::local_discovery::parse_models_response(&body))
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
    probe_client()?
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
        // the stem can only be a sidecar — not a transcript, and it must not
        // consume the scan budget or trigger resume advice (codex r2).
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
/// out of scope). Rows are informational; unparseable tails WARN with the
/// offending session keys (bounded).
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
        if inventory.corrupt_tail.is_empty() {
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
            "install a backend (macOS: sandbox-exec · Linux: bubblewrap · Windows: the \
             octos-sandbox helper · any: Docker), or set sandbox.fail_closed=true to \
             refuse instead of running unconfined",
        )
    }
}

/// Installed skills: count manifests under `<data_dir>/skills/*/manifest.json`
/// and flag unparseable ones. Read-only — no gating probes, no spawning.
/// The bundled embedding model (docs/THIRD_PARTY_MODELS.md): present and
/// complete, absent-but-fetchable, or disabled.
fn embedder_check(data_dir: &Path) -> Check {
    use crate::embed_model as em;
    if !cfg!(feature = "embed-llama") {
        return Check::warn(
            CAT_STORES,
            "embedding model",
            "this build has no in-process embedder (feature embed-llama); memory search is keyword-only",
            "rebuild with the default features (embed-llama) or configure a remote `embedding` provider",
        );
    }
    let status = em::model_status(data_dir);
    if status.complete {
        return Check::pass(
            CAT_STORES,
            "embedding model",
            format!("EmbeddingGemma-300M ready at {}", status.path.display()),
        );
    }
    if em::downloads_allowed(None) {
        Check::pass(
            CAT_STORES,
            "embedding model",
            format!(
                "EmbeddingGemma-300M not downloaded yet ({} MB, fetched on first use; `octos memory embedder --fetch` to do it now)",
                em::DEFAULT_MODEL_BYTES / (1024 * 1024)
            ),
        )
    } else {
        Check::warn(
            CAT_STORES,
            "embedding model",
            "absent and automatic download is disabled — memory search is keyword-only",
            "run `octos memory embedder --fetch`, or unset OCTOS_NO_MODEL_DOWNLOAD / embedding.auto_download",
        )
    }
}

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
