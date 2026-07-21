//! Project static-check tool (#1772, lite scope).
//!
//! Runs the project's cheapest native static check and returns COMPACT
//! diagnostics the LLM can act on immediately after an edit — the same
//! feedback-loop motivation as full LSP integration (#1772), without
//! spawning language servers:
//!
//! - `Cargo.toml`                    → `cargo check --message-format=json`
//! - `tsconfig.json`/`package.json`  → `tsc --noEmit`
//! - `go.mod`                        → `go vet ./...`
//!
//! Output is normalized to `file:line: level: message` lines, capped at
//! [`MAX_DIAGNOSTICS`] with a `... and N more` trailer. A missing checker
//! binary is a VALID answer (`success: true`, "checker not installed"), not
//! a tool failure — agents run in environments where toolchains may be
//! absent, and the model should move on rather than retry.
//!
//! The child process is spawned directly (argv array, no shell), with the
//! working directory pinned to the (optionally `path`-scoped) workspace,
//! stdin nulled, environment sanitized via the shared
//! [`crate::sandbox::BLOCKED_ENV_VARS`] denylist
//! ([`sanitize_command_env`]), and a hard [`DEFAULT_CHECK_TIMEOUT`] with
//! kill-by-PID on expiry (mirrors `tools/shell.rs`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tokio::time::timeout;

use super::{ConcurrencyClass, Tool, ToolResult};
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
const RAW_TAIL_CHARS: usize = 2000;

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
fn detect_project(root: &Path) -> Option<ProjectKind> {
    if root.join("Cargo.toml").is_file() {
        return Some(ProjectKind::Rust);
    }
    if root.join("tsconfig.json").is_file() || root.join("package.json").is_file() {
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

/// Parse `cargo check --message-format=json` stdout: one JSON object per
/// line; keep `compiler-message` entries at level error/warning, rendered
/// as `file:line: level[code]: message` from the primary span.
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
fn parse_tsc_line(line: &str) -> Option<(String, Level)> {
    // `src/index.ts(12,5): error TS2304: Cannot find name 'foo'.`
    let open = line.find('(')?;
    let close = open + line[open..].find(')')?;
    let (line_no, _col) = line[open + 1..close].split_once(',')?;
    let line_no: u64 = line_no.trim().parse().ok()?;
    let rest = line[close + 1..].strip_prefix(": ")?;
    let level = if rest.starts_with("error") {
        Level::Error
    } else if rest.starts_with("warning") {
        Level::Warning
    } else {
        return None;
    };
    let file = &line[..open];
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
        // still sees WHY.
        let code = exit_code.map_or_else(|| "unknown".to_string(), |c| c.to_string());
        let tail = raw_tail.map(str::trim).unwrap_or("");
        if tail.is_empty() {
            return format!("{label}: exit code {code}, no diagnostics reported");
        }
        return format!(
            "{label}: exit code {code}, no parseable diagnostics. Output tail:\n{tail}"
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
}

impl CheckTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: cwd.into(),
            resolve_binary: Arc::new(default_resolve_binary),
            timeout: DEFAULT_CHECK_TIMEOUT,
        }
    }

    /// Override the checker child-process timeout (default 120s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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
         Detects the project type from marker files: Cargo.toml -> `cargo check`, tsconfig.json/package.json -> `tsc --noEmit`, \
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
                "no supported project detected at {} (looked for Cargo.toml, tsconfig.json, package.json, go.mod)",
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

        // Spawn the checker: argv array (no shell), scoped cwd, nulled
        // stdin, sanitized environment (BLOCKED_ENV_VARS + secret strip).
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(kind.checker_args())
            .current_dir(&scope_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
                kill_by_pid(child_pid);
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

/// Kill a timed-out checker by PID (process group first, then the direct
/// PID) — `wait_with_output` consumed the [`tokio::process::Child`], so
/// PID-based kill is the only handle left. Mirrors `tools/shell.rs`.
fn kill_by_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
        let _ = StdCommand::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
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
    fn should_detect_typescript_project_when_tsconfig_or_package_json_present() {
        let with_tsconfig = tempdir();
        touch(with_tsconfig.path(), "tsconfig.json");
        assert_eq!(
            detect_project(with_tsconfig.path()),
            Some(ProjectKind::TypeScript)
        );

        let with_package_json = tempdir();
        touch(with_package_json.path(), "package.json");
        assert_eq!(
            detect_project(with_package_json.path()),
            Some(ProjectKind::TypeScript)
        );
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

    // ---- trait surface ----

    #[test]
    fn should_be_exclusive_concurrency_class() {
        let tool = CheckTool::new("/tmp");
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
        assert_eq!(tool.name(), "check");
    }
}
