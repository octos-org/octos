//! Project static-check tool (#1772, lite scope).
//!
//! Runs the project's cheapest native static check and returns COMPACT
//! diagnostics the LLM can act on immediately after an edit — the same
//! feedback-loop motivation as full LSP integration (#1772), without
//! spawning language servers:
//!
//! - `Cargo.toml`     → `cargo check --message-format=json`
//! - `tsconfig.json`  → `tsc --noEmit`
//! - `go.mod`         → `go vet ./...`
//!
//! A bare `package.json` is deliberately NOT a TypeScript marker: `tsc
//! --noEmit` without a tsconfig only prints help text and exits 1, and it
//! would shadow `go.mod` in Go repos that carry a tooling-only package.json.
//!
//! Output is normalized to `file:line: level: message` lines, capped at
//! [`MAX_DIAGNOSTICS`] with a `... and N more` trailer. A missing checker
//! binary is a VALID answer (`success: true`, "checker not installed"), not
//! a tool failure — agents run in environments where toolchains may be
//! absent, and the model should move on rather than retry.
//!
//! The child process runs with the working directory pinned to the
//! (optionally `path`-scoped) workspace, stdin nulled, environment
//! sanitized via the shared [`crate::sandbox::BLOCKED_ENV_VARS`] denylist
//! ([`sanitize_command_env`]), in its own process group, under a hard
//! [`DEFAULT_CHECK_TIMEOUT`] with an escalating whole-group kill (SIGTERM →
//! grace → liveness probe → SIGKILL) on expiry — mirrors `tools/shell.rs`.
//!
//! A real (non-no-op) session [`Sandbox`] confines the checker via
//! [`Sandbox::wrap_command`] exactly like the shell/exec/bash tools:
//! `cargo check` executes build.rs and proc-macros (arbitrary
//! project-controlled code) and `tsc` is preferred from the
//! workspace-writable `node_modules/.bin`, so this is precisely the command
//! class the sandbox exists to confine (#1607). Backends that cannot wrap
//! safely (Windows `cmd /C`, Docker with host-only checker paths) SKIP the
//! check as a valid answer instead of escaping to the host. Without a real
//! sandbox the checker is spawned directly (argv array, no shell).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tokio::time::timeout;

use super::{ConcurrencyClass, Tool, ToolResult};
use crate::sandbox::{NoSandbox, Sandbox};
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};

/// Maximum diagnostics included in the rendered report; the rest are
/// counted in a `... and N more diagnostics` trailer.
const MAX_DIAGNOSTICS: usize = 50;

/// Hard cap (bytes, UTF-8 safe) for a single rendered diagnostic line —
/// huge type names in rustc messages must not blow up the tool result.
const MAX_LINE_BYTES: usize = 500;

/// Hard timeout for the checker child process.
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// Tail of raw checker output surfaced when the checker fails without any
/// parseable diagnostics (e.g. a broken `Cargo.toml` manifest).
// 4000 chars ≈ the last ~50 lines: enough to carry a full cargo hard
// failure (manifest error + notes) without re-truncating the part that
// names the cause.
const RAW_TAIL_CHARS: usize = 4000;

/// Resolver from checker binary name (+ project root) to an executable
/// path. Injectable so tests can simulate a missing or fake checker.
type BinaryResolver = Arc<dyn Fn(&str, &Path) -> Option<PathBuf> + Send + Sync>;

/// Supported project kinds, in detection-priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectKind {
    Rust,
    TypeScript,
    Go,
}

impl ProjectKind {
    /// Checker binary looked up on PATH (via `which`/`where`).
    fn checker_binary(self) -> &'static str {
        match self {
            ProjectKind::Rust => "cargo",
            ProjectKind::TypeScript => "tsc",
            ProjectKind::Go => "go",
        }
    }

    /// Argv (after the binary) for the check invocation.
    fn checker_args(self) -> &'static [&'static str] {
        match self {
            ProjectKind::Rust => &["check", "--message-format=json"],
            ProjectKind::TypeScript => &["--noEmit"],
            ProjectKind::Go => &["vet", "./..."],
        }
    }

    /// Human-readable label used in the report header.
    fn label(self) -> &'static str {
        match self {
            ProjectKind::Rust => "cargo check",
            ProjectKind::TypeScript => "tsc --noEmit",
            ProjectKind::Go => "go vet",
        }
    }
}

/// Parsed, deduplicated diagnostics plus level counts (pre-cap).
#[derive(Debug, Default)]
struct ParsedDiagnostics {
    /// Rendered `file:line: level: message` lines, in first-seen order.
    lines: Vec<String>,
    /// Diagnostics counted as errors.
    errors: usize,
    /// Diagnostics counted as warnings.
    warnings: usize,
}

/// Detect the project kind by marker files directly under `root`.
/// Priority: Rust, then TypeScript, then Go (first match wins).
///
/// The TypeScript lane requires `tsconfig.json` — a bare `package.json` is
/// NOT enough: `tsc --noEmit` without a tsconfig just prints ~140 lines of
/// help text and exits 1 (never a useful answer), and treating package.json
/// as a marker would shadow `go.mod` in the very common "Go service +
/// tooling-only package.json" repo shape.
fn detect_project(root: &Path) -> Option<ProjectKind> {
    if root.join("Cargo.toml").is_file() {
        return Some(ProjectKind::Rust);
    }
    if root.join("tsconfig.json").is_file() {
        return Some(ProjectKind::TypeScript);
    }
    if root.join("go.mod").is_file() {
        return Some(ProjectKind::Go);
    }
    None
}

impl ParsedDiagnostics {
    /// Push a rendered line (deduplicated — cargo re-emits the same
    /// diagnostic once per compile target) and bump the level counter.
    /// Inner newlines are flattened so one diagnostic stays one line, and
    /// each line is truncated to [`MAX_LINE_BYTES`].
    fn push(&mut self, seen: &mut std::collections::HashSet<String>, line: String, level: Level) {
        let mut line = if line.contains('\n') {
            line.replace('\n', " ")
        } else {
            line
        };
        octos_core::truncate_utf8(&mut line, MAX_LINE_BYTES, "…");
        if !seen.insert(line.clone()) {
            return;
        }
        match level {
            Level::Error => self.errors += 1,
            Level::Warning => self.warnings += 1,
        }
        self.lines.push(line);
    }
}

/// Diagnostic severity bucket for counting.
#[derive(Debug, Clone, Copy)]
enum Level {
    Error,
    Warning,
}

/// Whether a rustc compiler-message is a run SUMMARY rather than a real
/// diagnostic: `aborting due to N previous errors[; M warnings emitted]`,
/// `N warnings emitted`, and the trailing help pointers. These recap
/// diagnostics already emitted individually.
fn is_cargo_summary_message(text: &str) -> bool {
    text.starts_with("aborting due to ")
        || text.ends_with(" warnings emitted")
        || text.ends_with(" warning emitted")
        || text.starts_with("Some errors have detailed explanations")
        || text.starts_with("For more information about")
}

/// Parse `cargo check --message-format=json` stdout: one JSON object per
/// line; keep `compiler-message` entries at level error/warning, rendered
/// as `file:line: level[code]: message` from the primary span. Run-summary
/// recaps (see [`is_cargo_summary_message`]) are skipped — they are not
/// diagnostics.
fn parse_cargo_json(stdout: &str) -> ParsedDiagnostics {
    let mut out = ParsedDiagnostics::default();
    let mut seen = std::collections::HashSet::new();

    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // non-JSON chatter
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let Some(level_str) = message.get("level").and_then(|l| l.as_str()) else {
            continue;
        };
        // "error", "error: internal compiler error" → error; notes/help skipped.
        let level = if level_str.starts_with("error") {
            Level::Error
        } else if level_str == "warning" {
            Level::Warning
        } else {
            continue;
        };
        let Some(text) = message.get("message").and_then(|m| m.as_str()) else {
            continue;
        };
        // rustc forwards run SUMMARY lines as compiler-messages too
        // ("aborting due to N previous errors", "N warnings emitted"). They
        // describe diagnostics already counted above — rendering them would
        // double-count ("2 errors" shows 3 entries) and burn cap slots.
        if is_cargo_summary_message(text) {
            continue;
        }
        let code = message
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str());
        let level_rendered = match (level, code) {
            (Level::Error, Some(code)) => format!("error[{code}]"),
            (Level::Error, None) => "error".to_string(),
            (Level::Warning, Some(code)) => format!("warning[{code}]"),
            (Level::Warning, None) => "warning".to_string(),
        };

        // Primary span (fallback: first span; fallback: no location).
        let spans = message.get("spans").and_then(|s| s.as_array());
        let span = spans.and_then(|spans| {
            spans
                .iter()
                .find(|s| s.get("is_primary").and_then(|p| p.as_bool()) == Some(true))
                .or_else(|| spans.first())
        });
        let rendered = match span {
            Some(span) => {
                let file = span
                    .get("file_name")
                    .and_then(|f| f.as_str())
                    .unwrap_or("<unknown>");
                let line_no = span.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0);
                format!("{file}:{line_no}: {level_rendered}: {text}")
            }
            None => format!("{level_rendered}: {text}"),
        };
        out.push(&mut seen, rendered, level);
    }
    out
}

/// Parse `tsc --noEmit` output lines of the shape
/// `path(line,col): error TS1234: message` into `path:line: error TS1234: message`.
/// Lines that don't match (progress, `Found N errors.` summaries) are skipped.
fn parse_tsc_output(stdout: &str) -> ParsedDiagnostics {
    let mut out = ParsedDiagnostics::default();
    let mut seen = std::collections::HashSet::new();

    for raw in stdout.lines() {
        let line = raw.trim_end();
        let Some((rendered, level)) = parse_tsc_line(line) else {
            continue;
        };
        out.push(&mut seen, rendered, level);
    }
    out
}

/// Parse one tsc diagnostic line; `None` when the line is not a diagnostic.
///
/// Anchors on the first `"): "` separator and scans BACK to its matching
/// `(` — anchoring on the first `(` of the whole line would silently drop
/// diagnostics whose file path itself contains parentheses (e.g. Next.js
/// route groups: `app/(group)/page.ts(1,7): error TS2322: ...`).
fn parse_tsc_line(line: &str) -> Option<(String, Level)> {
    // `src/index.ts(12,5): error TS2304: Cannot find name 'foo'.`
    let close = line.find("): ")?;
    let open = line[..close].rfind('(')?;
    let (line_no, col) = line[open + 1..close].split_once(',')?;
    let line_no: u64 = line_no.trim().parse().ok()?;
    // Both location fields must be numeric — rejects prose that merely
    // contains a `(...): ` fragment (help text, watch-mode banners).
    let _: u64 = col.trim().parse().ok()?;
    let rest = line[close + 1..].strip_prefix(": ")?;
    let level = if rest.starts_with("error") {
        Level::Error
    } else if rest.starts_with("warning") {
        Level::Warning
    } else {
        return None;
    };
    let file = &line[..open];
    if file.is_empty() {
        return None;
    }
    Some((format!("{file}:{line_no}: {rest}"), level))
}

/// Parse `go vet ./...` stderr: keep `path:line:col: message` finding lines
/// (vet has no levels — findings count as errors), skip `# package` headers
/// and empty lines.
fn parse_go_vet_output(stderr: &str) -> ParsedDiagnostics {
    let mut out = ParsedDiagnostics::default();
    let mut seen = std::collections::HashSet::new();

    for raw in stderr.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Findings look like `./main.go:10:2: msg` or `vet: helper.go:4:6: msg`
        // — require a `:<digits>:` segment so loader chatter is skipped.
        if !has_line_number_segment(line) {
            continue;
        }
        out.push(&mut seen, line.to_string(), Level::Error);
    }
    out
}

/// Whether the line contains a `:<digits>:` segment (file:line:col shape).
fn has_line_number_segment(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(colon) = line[i..].find(':') {
        let start = i + colon + 1;
        let digits = bytes[start..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits > 0 && bytes.get(start + digits) == Some(&b':') {
            return true;
        }
        i = start;
    }
    false
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Name the LAYER a non-diagnostic failure happened in, when the raw output
/// makes it recognizable.
///
/// "Exit code 1, no parseable diagnostics" reads as "the code is somehow
/// unverifiable" — but every observed instance of that message was the
/// environment, not the project: a sandbox denying rustup's settings lock,
/// a missing toolchain, a dead registry. The model (and the user) act on
/// the failing layer only if the report names it; the observed alternative
/// was a session that shipped 15 uncompiled files because "no parseable
/// diagnostics" told it nothing was actionable.
fn classify_failure(raw: &str) -> Option<&'static str> {
    // All three kernel spellings of a sandbox denial: EPERM (macOS
    // seatbelt), EACCES (Landlock), EROFS (bwrap/Docker read-only). The
    // note about "not recognized" ordering below matters: this arm runs
    // FIRST because a denial often cascades into secondary errors that
    // would otherwise match the later arms.
    if raw.contains("Operation not permitted")
        || raw.contains("Permission denied")
        || raw.contains("Read-only file system")
    {
        return Some(
            "the environment denied a file access (on a sandboxed run this is usually the \
             sandbox — on macOS, toolchain caches like ~/.rustup and ~/.cargo need \
             `sandbox.allow_toolchains`), not a defect in the project's code",
        );
    }
    if raw.contains("could not find `Cargo.toml`")
        || raw.contains("no `tsconfig.json`")
        || raw.contains("go.mod file not found")
    {
        return Some("the checker found no project manifest here — likely the wrong directory");
    }
    if raw.contains("command not found")
        || raw.contains("No such file or directory")
        || raw.contains("not recognized as an internal or external command")
    {
        return Some("the checker binary or toolchain itself is missing from this environment");
    }
    if raw.contains("failed to download")
        || raw.contains("Connection refused")
        || raw.contains("could not resolve host")
        || raw.contains("network failure")
        || raw.contains("failed to fetch")
    {
        return Some("the registry/network is unreachable, not a defect in the project's code");
    }
    None
}

/// Render the compact report: header with counts, up to [`MAX_DIAGNOSTICS`]
/// lines, `... and N more diagnostics` trailer. Falls back to `raw_tail`
/// when the checker failed without parseable diagnostics.
fn render_report(
    label: &str,
    diags: &ParsedDiagnostics,
    exit_code: Option<i32>,
    raw_tail: Option<&str>,
) -> String {
    let exit_ok = exit_code == Some(0);
    if diags.lines.is_empty() {
        if exit_ok {
            return format!("{label}: clean (no errors or warnings)");
        }
        // Checker failed without a single parseable diagnostic (e.g. broken
        // manifest / bad tsconfig): surface the raw output tail so the model
        // still sees WHY, and name the failing layer when it is
        // recognizable.
        let code = exit_code.map_or_else(|| "unknown".to_string(), |c| c.to_string());
        let tail = raw_tail.map(str::trim).unwrap_or("");
        if tail.is_empty() {
            return format!("{label}: exit code {code}, no diagnostics reported");
        }
        let layer = classify_failure(tail)
            .map(|hint| format!(" — {hint}"))
            .unwrap_or_default();
        return format!(
            "{label}: exit code {code}, no parseable diagnostics{layer}. Output tail:\n{tail}"
        );
    }

    let mut report = format!(
        "{label}: {}, {}\n\n",
        plural(diags.errors, "error"),
        plural(diags.warnings, "warning"),
    );
    for line in diags.lines.iter().take(MAX_DIAGNOSTICS) {
        report.push_str(line);
        report.push('\n');
    }
    let remaining = diags.lines.len().saturating_sub(MAX_DIAGNOSTICS);
    if remaining > 0 {
        report.push_str(&format!("... and {remaining} more diagnostics\n"));
    }
    report
}

/// Default binary resolver: workspace-local `node_modules/.bin/tsc` first
/// for TypeScript, then PATH lookup via the `which` crate (`where` on
/// Windows is handled by `which` internally).
fn default_resolve_binary(name: &str, root: &Path) -> Option<PathBuf> {
    if name == "tsc" {
        let local = root.join("node_modules").join(".bin").join("tsc");
        if local.is_file() {
            return Some(local);
        }
        #[cfg(windows)]
        {
            let local_cmd = root.join("node_modules").join(".bin").join("tsc.cmd");
            if local_cmd.is_file() {
                return Some(local_cmd);
            }
        }
    }
    which::which(name).ok()
}

#[derive(Deserialize)]
struct CheckArgs {
    /// Optional directory (relative to the workspace root) to scope the
    /// check; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
}

/// Tool that runs the project's cheap static check (`cargo check` /
/// `tsc --noEmit` / `go vet`) and returns compact diagnostics.
pub struct CheckTool {
    working_dir: PathBuf,
    resolve_binary: BinaryResolver,
    timeout: Duration,
    sandbox: Arc<dyn Sandbox>,
}

impl CheckTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: cwd.into(),
            resolve_binary: Arc::new(default_resolve_binary),
            timeout: DEFAULT_CHECK_TIMEOUT,
            sandbox: Arc::new(NoSandbox),
        }
    }

    /// Override the checker child-process timeout (default 120s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Confine the checker to the session sandbox — kept in lockstep with
    /// the shell/exec/bash tools (see
    /// `ToolRegistry::with_builtins_and_permissions`). `cargo check` runs
    /// build.rs/proc-macros, so it must not escape a sandbox that would
    /// confine the equivalent `shell("cargo check")`.
    pub fn with_shared_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Inject a custom binary resolver (tests: simulate missing/fake checkers).
    #[cfg(test)]
    fn with_binary_resolver(mut self, resolver: BinaryResolver) -> Self {
        self.resolve_binary = resolver;
        self
    }
}

#[async_trait]
impl Tool for CheckTool {
    fn name(&self) -> &str {
        "check"
    }

    fn description(&self) -> &str {
        "Run the project's cheap static check and return compact diagnostics (file:line: level: message). \
         Detects the project type from marker files: Cargo.toml -> `cargo check`, tsconfig.json -> `tsc --noEmit`, \
         go.mod -> `go vet ./...`. Reports at most 50 diagnostics and counts the rest. \
         Use after edits to catch compile/type errors early, before running the full test suite."
    }

    fn tags(&self) -> &[&str] {
        &["code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional directory (relative to the workspace root) to scope the check; defaults to the workspace root"
                }
            },
            "required": []
        })
    }

    /// Exclusive: `cargo check` writes `target/` (and shares the build lock
    /// with `shell`-launched cargo), `tsc`/`go vet` write caches — running
    /// concurrently with other mutating tools would race.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let args: CheckArgs = serde_json::from_value(args.clone())
            .map_err(|e| eyre::eyre!("invalid arguments: {e}"))?;

        // Resolve the optional scoping path inside the workspace fence.
        let scope_dir = match args.path.as_deref() {
            Some(path) => match super::resolve_path(&self.working_dir, path) {
                Ok(resolved) => {
                    if !resolved.is_dir() {
                        return Ok(fail(format!(
                            "check error: '{path}' is not a directory (pass a directory to scope the check, or omit 'path' for the workspace root)"
                        )));
                    }
                    resolved
                }
                Err(e) => return Ok(fail(format!("check error: {e}"))),
            },
            None => self.working_dir.clone(),
        };

        // Detect the project type from marker files at the scoped root.
        let Some(kind) = detect_project(&scope_dir) else {
            return Ok(ok(format!(
                "no supported project detected at {} (looked for Cargo.toml, tsconfig.json, go.mod)",
                scope_dir.display()
            )));
        };

        // Missing checker binary is a VALID answer, not a tool failure.
        let binary_name = kind.checker_binary();
        let Some(binary) = (self.resolve_binary)(binary_name, &scope_dir) else {
            return Ok(ok(format!(
                "checker not installed: {binary_name} (needed for `{}`); static check skipped",
                kind.label()
            )));
        };

        // Spawn the checker with scoped cwd, nulled stdin, and a sanitized
        // environment (BLOCKED_ENV_VARS + secret strip). A real (non-no-op)
        // session sandbox MUST confine the checker exactly like the
        // shell/exec/bash tools — `cargo check` executes build.rs and
        // proc-macros (arbitrary project-controlled code) and `tsc` is
        // preferred from the workspace-writable node_modules/.bin. Mirrors
        // the #1607 validator rules: POSIX backends wrap via `sh -c` (argv
        // shell-quoted with shlex); Windows (`cmd /C` ignores POSIX quoting)
        // and Docker (host checker paths don't resolve in-container) cannot
        // wrap safely, so the check is SKIPPED as a valid answer — like a
        // missing checker — rather than escaping to the host. Without a
        // real sandbox the argv is spawned directly (no shell,
        // injection-safe).
        let real_sandbox = Some(&self.sandbox).filter(|s| !s.is_noop());
        let mut cmd = match real_sandbox {
            Some(sandbox) => {
                if cfg!(windows) || sandbox.is_docker() {
                    return Ok(ok(format!(
                        "check skipped: `{}` is not supported under the active sandbox backend \
                         (Windows/Docker); run the checker via the shell tool instead",
                        kind.label()
                    )));
                }
                let Some(binary_str) = binary.to_str() else {
                    return Ok(fail(format!(
                        "check error: checker path is not valid UTF-8: {}",
                        binary.display()
                    )));
                };
                let quoted = match shlex::try_join(
                    std::iter::once(binary_str).chain(kind.checker_args().iter().copied()),
                ) {
                    Ok(quoted) => quoted,
                    Err(_) => {
                        return Ok(fail(format!(
                            "check error: checker path contains a NUL byte: {}",
                            binary.display()
                        )));
                    }
                };
                sandbox.wrap_command(&quoted, &scope_dir)
            }
            None => {
                let mut cmd = tokio::process::Command::new(&binary);
                cmd.args(kind.checker_args()).current_dir(&scope_dir);
                cmd
            }
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so the timeout path can signal the WHOLE tree
        // (checker + rustc/linker grandchildren) with a negative-PID kill.
        // Without this the group kill targets a group the child was never
        // placed in (ESRCH no-op) and grandchildren keep compiling — and
        // holding the cargo build lock — after the tool reported "timed
        // out". Same convention as bash/exec_command/validators.
        #[cfg(unix)]
        cmd.process_group(0);
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Ok(fail(format!(
                    "failed to run {} ({}): {e}",
                    kind.label(),
                    binary.display()
                )));
            }
        };
        let child_pid = child.id();

        let output = match timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Ok(fail(format!("failed to run {}: {e}", kind.label())));
            }
            Err(_) => {
                kill_by_pid(child_pid).await;
                return Ok(fail(format!(
                    "{} timed out after {}s",
                    kind.label(),
                    self.timeout.as_secs()
                )));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let diags = match kind {
            ProjectKind::Rust => parse_cargo_json(&stdout),
            ProjectKind::TypeScript => parse_tsc_output(&stdout),
            ProjectKind::Go => parse_go_vet_output(&stderr),
        };

        // Raw fallback for a failed run with nothing parseable: stderr tail
        // first (cargo/go put hard failures there), then stdout.
        let raw = if stderr.trim().is_empty() {
            stdout.as_ref()
        } else {
            stderr.as_ref()
        };
        let raw_tail = tail_chars(raw, RAW_TAIL_CHARS);

        let report = render_report(kind.label(), &diags, output.status.code(), Some(raw_tail));
        // The check RAN and produced an answer — diagnostics (or a raw
        // failure tail) are the payload, not a tool failure. `success:
        // false` is reserved for infrastructure errors (bad args, spawn
        // failure, timeout) so an M8.8 serial batch is not cascaded-
        // cancelled just because the project has pre-existing warnings.
        Ok(ok(report))
    }
}

/// Successful tool result (the check produced an answer).
fn ok(output: String) -> ToolResult {
    ToolResult {
        output,
        success: true,
        ..Default::default()
    }
}

/// Failed tool result (infrastructure error: args/spawn/timeout).
fn fail(output: String) -> ToolResult {
    ToolResult {
        output,
        success: false,
        ..Default::default()
    }
}

/// Last `max_chars` characters of `text` (UTF-8 safe).
fn tail_chars(text: &str, max_chars: usize) -> &str {
    let count = text.chars().count();
    if count <= max_chars {
        return text;
    }
    let (cut, _) = text
        .char_indices()
        .nth(count - max_chars)
        .unwrap_or((0, ' '));
    &text[cut..]
}

/// Kill a timed-out checker by PID — `wait_with_output` consumed the
/// [`tokio::process::Child`], so PID-based kill is the only handle left.
///
/// The child was spawned in its own process group (`process_group(0)`), so
/// the negative-PID signals reach the WHOLE tree (cargo's rustc/linker
/// grandchildren, tsc workers). SIGTERM to the group + direct PID first
/// (graceful — lets cargo release the build lock), 500ms grace, then
/// SIGKILL gated on a liveness probe of the GROUP (not just the leader:
/// on Linux `dash` exits to SIGTERM immediately, and a leader-only probe
/// skipped the escalation while orphaned grandchildren lived on — #1781
/// CI). Negative PIDs are passed after `--`: GNU/procps `kill` otherwise
/// parses `-<pid>` as an option and the group signal is silently never
/// delivered (macOS accepted it, Linux did not).
async fn kill_by_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        use std::process::Command as StdCommand;
        let group = format!("-{pid}");
        let _ = StdCommand::new("kill").args(["-15", "--", &group]).status();
        let _ = StdCommand::new("kill")
            .args(["-15", &pid.to_string()])
            .status();

        tokio::time::sleep(Duration::from_millis(500)).await;

        // `kill -0 -- -pgid` succeeds while ANY member of the group is
        // alive, and cannot hit a recycled group while a member remains.
        let group_alive = StdCommand::new("kill")
            .args(["-0", "--", &group])
            .status()
            .is_ok_and(|s| s.success());
        if group_alive {
            let _ = StdCommand::new("kill").args(["-9", "--", &group]).status();
        }
        let leader_alive = StdCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success());
        if leader_alive {
            let _ = StdCommand::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").expect("write marker");
    }

    /// One cargo `compiler-message` JSON line with a primary span.
    fn cargo_line(file: &str, line: u32, level: &str, code: Option<&str>, message: &str) -> String {
        let code_json = match code {
            Some(c) => format!(r#"{{"code":"{c}","explanation":null}}"#),
            None => "null".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","package_id":"pkg 0.1.0","manifest_path":"Cargo.toml","target":{{"name":"pkg"}},"message":{{"rendered":"...","level":"{level}","message":"{message}","code":{code_json},"spans":[{{"file_name":"{file}","line_start":{line},"line_end":{line},"column_start":1,"column_end":2,"is_primary":true}}]}}}}"#
        )
    }

    // ---- project detection ----

    #[test]
    fn should_detect_rust_project_when_cargo_toml_present() {
        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        assert_eq!(detect_project(dir.path()), Some(ProjectKind::Rust));
    }

    #[test]
    fn should_detect_typescript_only_when_tsconfig_present() {
        let with_tsconfig = tempdir();
        touch(with_tsconfig.path(), "tsconfig.json");
        assert_eq!(
            detect_project(with_tsconfig.path()),
            Some(ProjectKind::TypeScript)
        );

        // A bare package.json is NOT a TypeScript project: `tsc --noEmit`
        // without a tsconfig just prints help text and exits 1 — never a
        // useful answer.
        let with_package_json = tempdir();
        touch(with_package_json.path(), "package.json");
        assert_eq!(detect_project(with_package_json.path()), None);
    }

    #[test]
    fn should_detect_go_when_go_mod_with_tooling_only_package_json() {
        // Very common Go-service repo shape: go.mod plus a husky/prettier-only
        // package.json at the root. The check must reach `go vet`, not tsc
        // help-text noise.
        let dir = tempdir();
        touch(dir.path(), "go.mod");
        touch(dir.path(), "package.json");
        assert_eq!(detect_project(dir.path()), Some(ProjectKind::Go));
    }

    #[test]
    fn should_detect_go_project_when_go_mod_present() {
        let dir = tempdir();
        touch(dir.path(), "go.mod");
        assert_eq!(detect_project(dir.path()), Some(ProjectKind::Go));
    }

    #[test]
    fn should_detect_nothing_when_no_markers_present() {
        let dir = tempdir();
        assert_eq!(detect_project(dir.path()), None);
    }

    #[test]
    fn should_prefer_rust_when_multiple_markers_present() {
        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        touch(dir.path(), "package.json");
        touch(dir.path(), "go.mod");
        assert_eq!(detect_project(dir.path()), Some(ProjectKind::Rust));
    }

    // ---- cargo JSON parsing ----

    #[test]
    fn should_parse_cargo_compiler_message_lines_when_json_fixture() {
        let fixture = [
            cargo_line("src/main.rs", 3, "error", Some("E0308"), "mismatched types"),
            // Duplicate of the first line (same diagnostic re-emitted for a
            // second target) — must be deduplicated.
            cargo_line("src/main.rs", 3, "error", Some("E0308"), "mismatched types"),
            cargo_line("src/lib.rs", 10, "warning", None, "unused variable: `x`"),
            // Non-diagnostic reasons and sub-error levels are skipped.
            r#"{"reason":"build-finished","success":false}"#.to_string(),
            cargo_line("src/lib.rs", 11, "note", None, "not reported"),
            // Junk / non-JSON output must not break parsing.
            "not json at all".to_string(),
        ]
        .join("\n");

        let parsed = parse_cargo_json(&fixture);
        assert_eq!(parsed.errors, 1, "deduped to a single error");
        assert_eq!(parsed.warnings, 1);
        assert_eq!(
            parsed.lines,
            vec![
                "src/main.rs:3: error[E0308]: mismatched types".to_string(),
                "src/lib.rs:10: warning: unused variable: `x`".to_string(),
            ]
        );
    }

    #[test]
    fn should_skip_cargo_run_summary_messages() {
        // #1772 review: rustc forwards run SUMMARIES as compiler-messages
        // too. Counting them double-reports ("1 error" shows 2+ entries) and
        // burns cap slots. VERBATIM shapes rustc emits (no spans, no code) —
        // deliberately NOT built with `cargo_line` so this fixture can't
        // drift with the helper.
        let summary = |level: &str, message: &str| {
            format!(
                r#"{{"reason":"compiler-message","package_id":"pkg 0.1.0","manifest_path":"Cargo.toml","target":{{"name":"pkg"}},"message":{{"rendered":"{message}\n","level":"{level}","message":"{message}","code":null,"spans":[]}}}}"#
            )
        };
        let fixture = [
            cargo_line("src/main.rs", 3, "error", Some("E0308"), "mismatched types"),
            cargo_line("src/lib.rs", 10, "warning", None, "unused variable: `x`"),
            summary(
                "error",
                "aborting due to 1 previous error; 1 warning emitted",
            ),
            summary("error", "aborting due to 2 previous errors"),
            summary("warning", "3 warnings emitted"),
            summary("warning", "1 warning emitted"),
            summary(
                "error",
                "Some errors have detailed explanations: E0308, E0432.",
            ),
            summary(
                "error",
                "For more information about an error, try `rustc --explain E0308`.",
            ),
        ]
        .join("\n");

        let parsed = parse_cargo_json(&fixture);
        assert_eq!(parsed.errors, 1, "summaries must not count as errors");
        assert_eq!(parsed.warnings, 1, "summaries must not count as warnings");
        assert_eq!(parsed.lines.len(), 2, "lines: {:?}", parsed.lines);
    }

    #[test]
    fn should_render_cargo_diagnostic_without_span_when_spans_empty() {
        let fixture = r#"{"reason":"compiler-message","message":{"level":"error","message":"linker failure","code":null,"spans":[]}}"#;
        let parsed = parse_cargo_json(fixture);
        assert_eq!(parsed.errors, 1);
        assert_eq!(parsed.lines, vec!["error: linker failure".to_string()]);
    }

    // ---- cap behavior ----

    #[test]
    fn should_cap_diagnostics_at_50_when_more_present() {
        let fixture: Vec<String> = (0..60)
            .map(|i| cargo_line("src/big.rs", i + 1, "error", None, &format!("problem {i}")))
            .collect();
        let parsed = parse_cargo_json(&fixture.join("\n"));
        assert_eq!(parsed.errors, 60);

        let report = render_report("cargo check", &parsed, Some(101), None);
        assert!(
            report.contains("60 errors"),
            "header must count ALL diagnostics: {report}"
        );
        assert!(report.contains("src/big.rs:50: error: problem 49"));
        assert!(
            !report.contains("problem 50"),
            "diagnostics past the cap must not be listed: {report}"
        );
        assert!(
            report.contains("... and 10 more diagnostics"),
            "capped remainder must be counted: {report}"
        );
    }

    #[test]
    fn should_render_clean_report_when_no_diagnostics_and_exit_ok() {
        let report = render_report("cargo check", &ParsedDiagnostics::default(), Some(0), None);
        assert!(
            report.contains("cargo check") && report.contains("clean"),
            "clean run must say so: {report}"
        );
    }

    #[test]
    fn should_include_raw_tail_when_checker_failed_without_diagnostics() {
        let report = render_report(
            "cargo check",
            &ParsedDiagnostics::default(),
            Some(101),
            Some("error: failed to parse manifest at `Cargo.toml`"),
        );
        assert!(
            report.contains("failed to parse manifest"),
            "raw tail must surface when nothing parsed: {report}"
        );
        assert!(report.contains("101"), "exit code surfaced: {report}");
    }

    // ---- tsc parsing ----

    #[test]
    fn should_parse_tsc_lines_when_diagnostics_present() {
        let fixture = "\
src/index.ts(12,5): error TS2304: Cannot find name 'foo'.
src/util.ts(3,1): warning TS6133: 'x' is declared but its value is never read.
Found 2 errors in 2 files.
";
        let parsed = parse_tsc_output(fixture);
        assert_eq!(parsed.errors, 1);
        assert_eq!(parsed.warnings, 1);
        assert_eq!(
            parsed.lines,
            vec![
                "src/index.ts:12: error TS2304: Cannot find name 'foo'.".to_string(),
                "src/util.ts:3: warning TS6133: 'x' is declared but its value is never read."
                    .to_string(),
            ]
        );
    }

    #[test]
    fn should_parse_tsc_line_when_path_contains_parentheses() {
        // Next.js route groups etc.: the file path itself contains parens —
        // the parser must anchor on the `(line,col): ` location, not on the
        // first `(` of the line (which silently dropped the diagnostic).
        let fixture = "app/(group)/bad.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.";
        let parsed = parse_tsc_output(fixture);
        assert_eq!(parsed.errors, 1, "paren-path diagnostic must parse");
        assert_eq!(
            parsed.lines,
            vec![
                "app/(group)/bad.ts:1: error TS2322: Type 'string' is not assignable to type 'number'."
                    .to_string()
            ]
        );
    }

    #[test]
    fn should_skip_tsc_prose_lines_with_parenthesized_fragments() {
        // Help-text / prose with a `(...): ` shape but a non-numeric location
        // must not be mistaken for a diagnostic.
        let fixture = "Watching for file changes (press h, then q): error reporting is enabled.";
        let parsed = parse_tsc_output(fixture);
        assert_eq!(parsed.errors + parsed.warnings, 0, "{:?}", parsed.lines);
    }

    // ---- go vet parsing ----

    #[test]
    fn should_parse_go_vet_lines_when_findings_present() {
        let fixture = "\
# example.com/mymod
./main.go:10:2: unreachable code
vet: helper.go:4:6: undefined: missingFn
";
        let parsed = parse_go_vet_output(fixture);
        assert_eq!(parsed.errors, 2);
        assert_eq!(parsed.warnings, 0);
        assert_eq!(
            parsed.lines,
            vec![
                "./main.go:10:2: unreachable code".to_string(),
                "vet: helper.go:4:6: undefined: missingFn".to_string(),
            ]
        );
    }

    // ---- execute: missing binary ----

    #[tokio::test]
    async fn should_report_checker_not_installed_when_binary_absent() {
        let dir = tempdir();
        touch(dir.path(), "go.mod");
        let tool = CheckTool::new(dir.path()).with_binary_resolver(Arc::new(|_, _| None));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "missing checker is a valid answer, not a tool failure: {}",
            result.output
        );
        assert!(
            result.output.contains("checker not installed: go"),
            "must name the missing binary: {}",
            result.output
        );
    }

    // ---- execute: no project ----

    #[tokio::test]
    async fn should_report_no_project_when_markers_missing() {
        let dir = tempdir();
        let tool = CheckTool::new(dir.path());

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success, "no-project is a valid answer");
        assert!(
            result.output.contains("no supported project detected"),
            "must explain what was looked for: {}",
            result.output
        );
        assert!(result.output.contains("Cargo.toml"));
    }

    // ---- execute: arg validation ----

    #[tokio::test]
    async fn should_reject_path_outside_workspace_when_traversal_arg() {
        let dir = tempdir();
        let tool = CheckTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "../outside"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("check error"),
            "path errors surface as tool-level failures: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_reject_non_directory_path_when_file_given() {
        let dir = tempdir();
        std::fs::write(dir.path().join("Cargo.toml"), b"[package]").unwrap();
        let tool = CheckTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "Cargo.toml"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("not a directory"),
            "scoping path must be a directory: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_error_when_path_arg_has_wrong_type() {
        let dir = tempdir();
        let tool = CheckTool::new(dir.path());
        let err = tool.execute(&serde_json::json!({"path": 42})).await;
        assert!(err.is_err(), "wrong-typed args must be rejected");
    }

    // ---- execute: fake checker end-to-end (unix: shell-script fixture) ----

    #[cfg(unix)]
    #[tokio::test]
    async fn should_run_fake_checker_and_render_diagnostics() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");

        let line_a = cargo_line("src/main.rs", 3, "error", Some("E0308"), "mismatched types");
        let line_b = cargo_line("src/lib.rs", 7, "warning", None, "unused import: `foo`");
        let script = dir.path().join("fake-cargo.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho '{line_a}'\necho '{line_b}'\nexit 101\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let script_for_resolver = script.clone();
        let tool = CheckTool::new(dir.path()).with_binary_resolver(Arc::new(move |name, _| {
            (name == "cargo").then(|| script_for_resolver.clone())
        }));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "diagnostics found is a successful check run: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("src/main.rs:3: error[E0308]: mismatched types"),
            "rendered error line missing: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("src/lib.rs:7: warning: unused import: `foo`"),
            "rendered warning line missing: {}",
            result.output
        );
        assert!(
            result.output.contains("1 error") && result.output.contains("1 warning"),
            "header must count levels: {}",
            result.output
        );
    }

    // ---- execute: session-sandbox confinement ----

    /// Review #1772 (high): a real session sandbox must confine the checker
    /// exactly like shell/exec/bash — `cargo check` runs build.rs and
    /// proc-macros. The marker sandbox substitutes the wrapped command, so
    /// seeing the marker (and the shell-quoted argv) in the output proves
    /// the spawn went through `Sandbox::wrap_command`, not a direct spawn.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_run_checker_through_session_sandbox_when_real_sandbox_set() {
        struct MarkerSandbox;
        impl Sandbox for MarkerSandbox {
            fn wrap_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c")
                    .arg(format!("echo \"SANDBOX-WRAPPED: {command}\"; exit 7"))
                    .current_dir(cwd);
                cmd
            }
        }

        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        let tool = CheckTool::new(dir.path())
            .with_binary_resolver(Arc::new(|name, _| {
                (name == "cargo").then(|| PathBuf::from("/fake/cargo"))
            }))
            .with_shared_sandbox(Arc::new(MarkerSandbox));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "a sandboxed run that exits non-zero is still an answer: {}",
            result.output
        );
        // (shlex may quote individual argv elements — e.g.
        // `'--message-format=json'` — so match the pieces, not one exact
        // quoting.)
        assert!(
            result.output.contains("SANDBOX-WRAPPED: /fake/cargo check")
                && result.output.contains("--message-format=json"),
            "checker must be wrapped by the session sandbox (quoted argv): {}",
            result.output
        );
    }

    /// Review #1772 (high): backends that cannot wrap safely (Docker: host
    /// checker paths don't resolve in-container; Windows `cmd /C` ignores
    /// POSIX quoting) must SKIP the check as a valid answer — never fall
    /// back to an unconfined direct spawn on the host.
    #[tokio::test]
    async fn should_skip_check_when_sandbox_backend_cannot_wrap() {
        struct DockerLikeSandbox;
        impl Sandbox for DockerLikeSandbox {
            fn wrap_command(&self, _command: &str, _cwd: &Path) -> tokio::process::Command {
                panic!("check must not attempt to wrap under a Docker sandbox");
            }
            fn is_docker(&self) -> bool {
                true
            }
        }

        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        let tool = CheckTool::new(dir.path())
            .with_binary_resolver(Arc::new(|name, _| {
                (name == "cargo").then(|| PathBuf::from("/fake/cargo"))
            }))
            .with_shared_sandbox(Arc::new(DockerLikeSandbox));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "sandbox-unsupported skip is a valid answer, not a tool failure: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("not supported under the active sandbox backend"),
            "skip must explain why and point at the shell tool: {}",
            result.output
        );
    }

    /// #2196 review MUST-FIX regression: a REAL `AppContainerSandbox` whose
    /// helper is gone (true on CI runners — octos-sandbox.exe is absent)
    /// must take the same sandbox-unsupported SKIP as the mock above — never
    /// the `is_noop` -> direct-spawn transition, which would execute the
    /// workspace checker (build.rs / proc-macros: project-controlled code)
    /// raw on the host. The old dynamic `is_noop()` re-probe did exactly
    /// that. A direct spawn of the fake resolver path would surface as a
    /// spawn error, not this skip text.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_check_skips_when_appcontainer_helper_vanished() {
        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        let tool = CheckTool::new(dir.path())
            .with_binary_resolver(Arc::new(|name, _| {
                (name == "cargo").then(|| PathBuf::from("C:/fake/cargo.exe"))
            }))
            .with_shared_sandbox(Arc::new(crate::sandbox::AppContainerSandbox {
                allow_network: false,
                read_allow_paths: vec![],
                profile_name: None,
                workspace_write: true,
            }));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "sandbox-unsupported skip is a valid answer: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("not supported under the active sandbox backend"),
            "helper-vanished AppContainer must skip, never spawn directly: {}",
            result.output
        );
    }

    // ---- execute: timeout kills the whole checker process tree ----

    /// Review #1772 (high): without `process_group(0)` before spawn, the
    /// timeout path's negative-PID kill targets a process group the checker
    /// was never placed in (guaranteed ESRCH no-op) and only the direct PID
    /// dies — rustc/linker grandchildren keep compiling and hold the cargo
    /// build lock after the tool has reported "timed out". Mirrors
    /// `bash_kills_grandchildren_via_process_group_on_timeout`.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_kill_checker_grandchildren_when_timeout_fires() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        touch(dir.path(), "Cargo.toml");
        let sentinel = dir.path().join("grandchild-late.txt");
        // Backgrounded grandchild touches the sentinel after a sleep longer
        // than the timeout; `wait` keeps the checker alive so the timeout
        // path is forced to walk the process group.
        let script = dir.path().join("fake-cargo.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n(sleep 6; touch {}) & wait\n",
                sentinel.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let script_for_resolver = script.clone();
        let tool = CheckTool::new(dir.path())
            .with_binary_resolver(Arc::new(move |name, _| {
                (name == "cargo").then(|| script_for_resolver.clone())
            }))
            .with_timeout(Duration::from_millis(500));

        let started = std::time::Instant::now();
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            !result.success,
            "timeout is a tool failure: {}",
            result.output
        );
        assert!(
            result.output.contains("timed out"),
            "timeout must be reported: {}",
            result.output
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "check must return promptly on timeout (got {:?})",
            started.elapsed()
        );

        // Wait past when the orphaned grandchild's `touch` would fire.
        tokio::time::sleep(Duration::from_millis(7_000)).await;
        assert!(
            !sentinel.exists(),
            "grandchild must be killed via the checker's process group on \
             timeout — sentinel at {} should NOT exist",
            sentinel.display()
        );
    }

    // ---- trait surface ----

    #[test]
    fn should_be_exclusive_concurrency_class() {
        let tool = CheckTool::new("/tmp");
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
        assert_eq!(tool.name(), "check");
    }

    /// A non-diagnostic failure names the failing LAYER when recognizable:
    /// sandbox denial, missing manifest, missing toolchain, dead registry.
    /// The observed cost of the bare message was a session that treated
    /// "no parseable diagnostics" as "nothing actionable" while every
    /// cargo run was dying on a sandbox-denied rustup lock.
    #[test]
    fn should_name_failure_layer_when_recognizable() {
        let cases = [
            (
                "error: could not read settings file: '/Users/u/.rustup/settings.toml': Operation not permitted (os error 1)",
                "sandbox",
            ),
            (
                "error: could not find `Cargo.toml` in `/tmp` or any parent",
                "manifest",
            ),
            (
                "sh: cargo: command not found",
                "toolchain itself is missing",
            ),
            (
                "error: failed to download from `https://crates.io/...`",
                "registry/network",
            ),
        ];
        for (raw, expect) in cases {
            let report = render_report(
                "cargo check",
                &ParsedDiagnostics::default(),
                Some(1),
                Some(raw),
            );
            assert!(
                report.contains(expect),
                "expected layer '{expect}' named in: {report}"
            );
            assert!(
                report.contains(raw.trim()),
                "raw tail must still be present"
            );
        }
        // Unrecognized failures keep the plain message — no invented cause.
        let report = render_report(
            "cargo check",
            &ParsedDiagnostics::default(),
            Some(101),
            Some("thread panicked at ..."),
        );
        assert!(
            report.contains("no parseable diagnostics."),
            "no invented layer: {report}"
        );
    }
}
