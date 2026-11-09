//! Post-edit formatting (issue #1774).
//!
//! After a successful `edit_file` / `write_file` / `diff_edit`, the agent can
//! optionally run the language formatter for the file (rustfmt, prettier,
//! black, gofmt) so agent edits never leave formatting noise behind.
//!
//! Contract:
//! - **OFF by default** — opt in via `format_after_edit: true` in config,
//!   threaded through [`crate::AgentConfig::format_after_edit`] onto
//!   [`crate::tools::ToolContext::format_after_edit`].
//! - **File-scoped** — the formatter is invoked on exactly one file, never a
//!   directory.
//! - **Best-effort** — a missing binary, a formatter failure, or a timeout
//!   never fails the edit; at most a note is appended to the tool output.
//! - **Sandboxed env** — the child process env is sanitized through the same
//!   [`crate::sandbox::BLOCKED_ENV_VARS`] path the sandbox/MCP/hooks share
//!   (via [`crate::subprocess_env`]).
//! - **Hard timeout** — the formatter is killed after [`FORMAT_TIMEOUT`].
//! - **No stale mental copy** — when the formatter changed the file, the tool
//!   result echoes the re-read, formatted content so the LLM sees exactly
//!   what is on disk.
//!
//! # Trust model
//!
//! Enabling `format_after_edit` means **trusting the workspace's formatter
//! configuration**: the child runs with cwd = the file's parent, and real
//! formatters discover and honor project config found there (`rustfmt.toml`,
//! `.prettierrc`, `pyproject.toml`, ...). Prettier in particular can load
//! project-local plugins — JavaScript that executes with the agent's
//! privileges. Env sanitization strips secrets from the child, but it cannot
//! contain code the formatter itself chooses to run. Do not enable this
//! opt-in on untrusted workspaces.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};

/// Hard wall-clock limit for a single formatter run. The child is killed when
/// the limit elapses (`kill_on_drop` — dropping the wait future SIGKILLs).
///
/// #2053 — the budget is platform-scaled because it bounds a *hung* formatter,
/// not a slow one, and "slow" means something very different on Windows. A
/// cold `rustfmt` there pays for process creation plus on-access AV scanning of
/// the binary and its inputs, and routinely exceeds five seconds on CI: the
/// same test (`edit_file::should_return_formatted_content_when_format_after_edit_enabled`)
/// tripped this limit five times across four unrelated PRs in one day, twice
/// consecutively on the same PR, always with
///
/// ```text
/// Note: post-edit formatting with rustfmt timed out after 5s and was killed;
///       the edit itself was applied unchanged.
/// ```
///
/// This is not only a CI artifact. The timeout is a real code path: when it
/// fires, the edit lands unformatted and the user is merely told so. A budget
/// that a healthy formatter cannot meet therefore degrades Windows users'
/// output silently and at random, which is a worse failure than waiting a few
/// more seconds for the answer they asked for. Raising the ceiling does not
/// weaken the guarantee it exists for — a genuinely hung formatter is still
/// killed, just later.
///
/// Unix keeps 5s, where rustfmt returns in well under a second.
#[cfg(not(windows))]
pub const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Windows counterpart of [`FORMAT_TIMEOUT`] — see that constant for why the
/// budget differs by platform.
#[cfg(windows)]
pub const FORMAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Byte cap for the formatted-content echo appended to the tool output.
/// Larger files are truncated at a UTF-8 boundary with a marker; the LLM can
/// `read_file` for the rest.
const MAX_FORMATTED_ECHO_BYTES: usize = 16 * 1024;

/// A language formatter octos knows how to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatterKind {
    /// Rust — `rustfmt --edition 2024 <file>`.
    Rustfmt,
    /// JavaScript / TypeScript — `prettier --write <file>`.
    Prettier,
    /// Python — `black --quiet <file>`.
    Black,
    /// Go — `gofmt -w <file>`.
    Gofmt,
}

/// Concrete command line for a formatter invocation (file path appended last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatterCommand {
    /// Binary name looked up on PATH (`which` / `where`).
    pub program: String,
    /// Arguments placed before the target file path.
    pub args: Vec<String>,
}

impl FormatterKind {
    /// Detect the formatter for a file by extension (ASCII case-insensitive).
    /// `None` for unknown extensions or extension-less files.
    pub fn for_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rustfmt),
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Some(Self::Prettier),
            "py" | "pyi" => Some(Self::Black),
            "go" => Some(Self::Gofmt),
            _ => None,
        }
    }

    /// Short human-readable name used in tool-output notes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rustfmt => "rustfmt",
            Self::Prettier => "prettier",
            Self::Black => "black",
            Self::Gofmt => "gofmt",
        }
    }

    /// Default command line for this formatter (file path appended last).
    ///
    /// Note: prettier is invoked directly (never through `npx`, which could
    /// hit the network to install it) — the missing-binary skip in
    /// [`format_file_with_command`] keeps this offline-safe.
    pub fn command(self) -> FormatterCommand {
        let (program, args): (&str, &[&str]) = match self {
            // `skip_children=true` pins the FILE-scoped contract: rustfmt's
            // default traverses `mod` declarations and silently rewrites
            // child modules on disk — files the edit never targeted, whose
            // cache entries and git snapshots would then be stale (#1774
            // review).
            Self::Rustfmt => (
                "rustfmt",
                &["--edition", "2024", "--config", "skip_children=true"],
            ),
            Self::Prettier => ("prettier", &["--write"]),
            Self::Black => ("black", &["--quiet"]),
            Self::Gofmt => ("gofmt", &["-w"]),
        };
        FormatterCommand {
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }
}

/// Outcome of one post-edit formatting attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatOutcome {
    /// No formatter is mapped for this file's extension.
    NoFormatter,
    /// The formatter binary is not on PATH — silently skipped.
    MissingBinary { formatter: &'static str },
    /// The formatter ran and exited successfully.
    Formatted { formatter: &'static str },
    /// The formatter exited non-zero (or failed to spawn); the edit stands.
    Failed {
        formatter: &'static str,
        detail: String,
    },
    /// The formatter exceeded [`FORMAT_TIMEOUT`] and was killed.
    TimedOut { formatter: &'static str },
}

/// Check if a binary exists on PATH (`where` on Windows, `which` on Unix —
/// same convention as `skills.rs` / `sandbox/mod.rs`).
pub(crate) fn binary_on_path(bin: &str) -> bool {
    #[cfg(windows)]
    let prog = "where";
    #[cfg(not(windows))]
    let prog = "which";

    std::process::Command::new(prog)
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the sanitized, file-scoped formatter command.
///
/// - exactly ONE file argument, appended last (never a directory);
/// - cwd = the file's parent so project config (`rustfmt.toml`,
///   `.prettierrc`, ...) resolves naturally;
/// - env sanitized through the shared subprocess path, which strips every
///   [`crate::sandbox::BLOCKED_ENV_VARS`] entry and secret-named vars;
/// - `kill_on_drop` so a timed-out child is killed, not leaked.
fn build_command(cmd: &FormatterCommand, file: &Path) -> Command {
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args);
    command.arg(file);
    if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
        command.current_dir(parent);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    sanitize_command_env(&mut command, &EnvAllowlist::empty());
    command
}

/// Format a single file with its extension-mapped formatter, if any.
pub async fn format_file(path: &Path) -> FormatOutcome {
    let Some(kind) = FormatterKind::for_path(path) else {
        return FormatOutcome::NoFormatter;
    };
    format_file_with_command(path, kind.name(), &kind.command(), FORMAT_TIMEOUT).await
}

/// Testable core: run `cmd` against `path` with a hard `timeout`.
pub(crate) async fn format_file_with_command(
    path: &Path,
    formatter: &'static str,
    cmd: &FormatterCommand,
    timeout: Duration,
) -> FormatOutcome {
    if !binary_on_path(&cmd.program) {
        tracing::debug!(
            formatter,
            program = %cmd.program,
            "post-edit formatter binary not on PATH — skipping"
        );
        return FormatOutcome::MissingBinary { formatter };
    }

    let mut command = build_command(cmd, path);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return FormatOutcome::Failed {
                formatter,
                detail: format!("failed to spawn {}: {err}", cmd.program),
            };
        }
    };

    // `kill_on_drop(true)` is set in `build_command`: when the timeout wins
    // the race, the dropped `wait_with_output` future drops the child handle
    // and tokio kills the process — no orphaned formatter keeps mutating the
    // workspace after we reported a timeout.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => FormatOutcome::Formatted { formatter },
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut detail = output.status.to_string();
            let trimmed = stderr.trim();
            if !trimmed.is_empty() {
                detail.push_str(": ");
                detail.push_str(&octos_core::truncated_utf8(trimmed, 500, "..."));
            }
            FormatOutcome::Failed { formatter, detail }
        }
        Ok(Err(err)) => FormatOutcome::Failed {
            formatter,
            detail: format!("failed to collect {} output: {err}", cmd.program),
        },
        Err(_elapsed) => {
            tracing::warn!(
                formatter,
                timeout_secs = timeout.as_secs_f32(),
                path = %path.display(),
                "post-edit formatter exceeded timeout — killed"
            );
            FormatOutcome::TimedOut { formatter }
        }
    }
}

/// Run post-edit formatting for a just-written file and produce the note to
/// append to the tool output (starting with `\n\n`), or `None` when there is
/// nothing worth telling the LLM. Never fails the edit.
///
/// `written` is the content the tool just wrote — compared against the
/// re-read to decide whether the formatter changed anything.
pub async fn post_edit_format_note(path: &Path, written: &str) -> Option<String> {
    let outcome = format_file(path).await;
    note_for_outcome(path, written, outcome).await
}

/// Render the tool-output note for a formatting outcome. Re-reads the file on
/// [`FormatOutcome::Formatted`] so the echoed content is exactly what is on
/// disk.
pub(crate) async fn note_for_outcome(
    path: &Path,
    written: &str,
    outcome: FormatOutcome,
) -> Option<String> {
    match outcome {
        FormatOutcome::NoFormatter | FormatOutcome::MissingBinary { .. } => None,
        FormatOutcome::Formatted { formatter } => {
            // Re-read from disk so the echo is exactly the formatter's
            // output, not our guess (same O_NOFOLLOW path as the tools).
            match crate::tools::read_no_follow(path).await {
                Ok(on_disk) if on_disk != written => {
                    let echo = octos_core::truncated_utf8(
                        &on_disk,
                        MAX_FORMATTED_ECHO_BYTES,
                        "\n... [formatted content truncated — use read_file for the rest]",
                    );
                    Some(format!(
                        "\n\nNote: {formatter} reformatted this file after the edit; the \
                         on-disk content differs from the text you submitted. Current file \
                         content:\n```\n{echo}\n```"
                    ))
                }
                // Byte-identical after formatting: the LLM's mental copy is
                // already accurate — stay silent.
                Ok(_) => None,
                Err(err) => Some(format!(
                    "\n\nNote: {formatter} reformatted this file after the edit, but \
                     re-reading it failed ({err}); use read_file to see the current content."
                )),
            }
        }
        FormatOutcome::Failed { formatter, detail } => Some(format!(
            "\n\nNote: post-edit formatting with {formatter} failed ({detail}); the edit \
             itself was applied unchanged."
        )),
        FormatOutcome::TimedOut { formatter } => Some(format!(
            "\n\nNote: post-edit formatting with {formatter} timed out after {}s and was \
             killed; the edit itself was applied unchanged.",
            FORMAT_TIMEOUT.as_secs()
        )),
    }
}
