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
pub const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ------------------------------------------------------------------
    // Language detection
    // ------------------------------------------------------------------

    #[test]
    fn should_detect_language_when_known_extension() {
        let cases = [
            ("src/main.rs", FormatterKind::Rustfmt),
            ("web/app.ts", FormatterKind::Prettier),
            ("web/App.tsx", FormatterKind::Prettier),
            ("web/index.js", FormatterKind::Prettier),
            ("web/Comp.jsx", FormatterKind::Prettier),
            ("web/util.mjs", FormatterKind::Prettier),
            ("web/util.cjs", FormatterKind::Prettier),
            ("tools/gen.py", FormatterKind::Black),
            ("tools/typed.pyi", FormatterKind::Black),
            ("cmd/serve.go", FormatterKind::Gofmt),
        ];
        for (path, expected) in cases {
            assert_eq!(
                FormatterKind::for_path(Path::new(path)),
                Some(expected),
                "{path} should map to {expected:?}"
            );
        }
    }

    #[test]
    fn should_detect_no_language_when_unknown_or_missing_extension() {
        for path in ["notes.txt", "Makefile", "archive.tar.gz", "noext"] {
            assert_eq!(
                FormatterKind::for_path(Path::new(path)),
                None,
                "{path} should have no formatter"
            );
        }
    }

    #[test]
    fn should_detect_language_when_uppercase_extension() {
        assert_eq!(
            FormatterKind::for_path(Path::new("LEGACY.RS")),
            Some(FormatterKind::Rustfmt)
        );
        assert_eq!(
            FormatterKind::for_path(Path::new("SCRIPT.PY")),
            Some(FormatterKind::Black)
        );
    }

    // ------------------------------------------------------------------
    // Command mapping
    // ------------------------------------------------------------------

    #[test]
    fn should_map_expected_formatter_commands() {
        let rust = FormatterKind::Rustfmt.command();
        assert_eq!(rust.program, "rustfmt");
        // skip_children pins the FILE-scoped contract (#1774 review):
        // rustfmt's default traverses `mod` declarations and rewrites child
        // modules the edit never targeted.
        assert_eq!(
            rust.args,
            vec![
                "--edition".to_string(),
                "2024".to_string(),
                "--config".to_string(),
                "skip_children=true".to_string(),
            ]
        );

        let prettier = FormatterKind::Prettier.command();
        assert_eq!(prettier.program, "prettier");
        assert_eq!(prettier.args, vec!["--write".to_string()]);

        let black = FormatterKind::Black.command();
        assert_eq!(black.program, "black");
        assert_eq!(black.args, vec!["--quiet".to_string()]);

        let gofmt = FormatterKind::Gofmt.command();
        assert_eq!(gofmt.program, "gofmt");
        assert_eq!(gofmt.args, vec!["-w".to_string()]);
    }

    // ------------------------------------------------------------------
    // Command construction: env sanitization + file scoping
    // ------------------------------------------------------------------

    #[test]
    fn should_build_command_with_blocked_env_vars_removed() {
        // The formatter child MUST go through the same BLOCKED_ENV_VARS
        // sanitization the sandbox/MCP/hooks paths share. `env_remove`
        // entries surface via `get_envs()` as `(name, None)`.
        let file = PathBuf::from("/tmp/octos-format-test/main.rs");
        let cmd = build_command(&FormatterKind::Rustfmt.command(), &file);
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().to_string())
            .collect();
        for blocked in crate::sandbox::BLOCKED_ENV_VARS {
            assert!(
                removed.iter().any(|name| name == blocked),
                "{blocked} must be explicitly removed from the formatter env"
            );
        }
    }

    #[test]
    fn should_scope_command_to_single_file() {
        // FILE-scoped, never directory-wide: the one and only path argument
        // is the target file, appended last.
        let file = PathBuf::from("/tmp/octos-format-test/main.rs");
        let cmd = build_command(&FormatterKind::Gofmt.command(), &file);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-w".to_string(), file.display().to_string()]);
    }

    // ------------------------------------------------------------------
    // Execution outcomes (missing binary, failure, success, timeout kill)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn should_report_missing_binary_when_program_not_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main(){}\n").unwrap();

        let cmd = FormatterCommand {
            program: "octos-formatter-that-does-not-exist-1774".to_string(),
            args: vec![],
        };
        let outcome = format_file_with_command(&file, "rustfmt", &cmd, FORMAT_TIMEOUT).await;
        assert_eq!(
            outcome,
            FormatOutcome::MissingBinary {
                formatter: "rustfmt"
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_report_formatted_when_formatter_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main(){}\n").unwrap();

        // `sh -c :` — no-op success; the appended file path lands in $0.
        let cmd = FormatterCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), ":".to_string()],
        };
        let outcome = format_file_with_command(&file, "rustfmt", &cmd, FORMAT_TIMEOUT).await;
        assert_eq!(
            outcome,
            FormatOutcome::Formatted {
                formatter: "rustfmt"
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_report_failure_when_formatter_exits_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main(){}\n").unwrap();

        let cmd = FormatterCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "echo boom >&2; exit 3".to_string()],
        };
        let outcome = format_file_with_command(&file, "rustfmt", &cmd, FORMAT_TIMEOUT).await;
        match outcome {
            FormatOutcome::Failed { formatter, detail } => {
                assert_eq!(formatter, "rustfmt");
                assert!(detail.contains("boom"), "stderr should surface: {detail}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_kill_formatter_when_timeout_exceeded() {
        // The hanging "formatter" would create a marker via $1 (the appended
        // file path) after 2s. With a 250ms hard timeout the child must be
        // killed: TimedOut now AND the marker never appears.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.rs");

        let cmd = FormatterCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "sleep 2; echo killed-me-not > \"$1\"".to_string(),
                "--".to_string(),
            ],
        };
        let started = std::time::Instant::now();
        let outcome =
            format_file_with_command(&marker, "rustfmt", &cmd, Duration::from_millis(250)).await;
        assert_eq!(
            outcome,
            FormatOutcome::TimedOut {
                formatter: "rustfmt"
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must fire well before the child would finish"
        );

        // Give the would-be write ample time; the kill must have prevented it.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !marker.exists(),
            "timed-out formatter child must be killed, not left running"
        );
    }

    // ------------------------------------------------------------------
    // Note rendering (LLM-facing summary)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn should_render_no_note_when_no_formatter_or_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();

        assert_eq!(
            note_for_outcome(&file, "hello\n", FormatOutcome::NoFormatter).await,
            None
        );
        assert_eq!(
            note_for_outcome(
                &file,
                "hello\n",
                FormatOutcome::MissingBinary {
                    formatter: "rustfmt"
                }
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn should_render_formatted_content_when_formatter_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        // On-disk content is the (post-format) version...
        std::fs::write(&file, "fn main() {}\n").unwrap();

        // ...which differs from what the tool wrote pre-format.
        let note = note_for_outcome(
            &file,
            "fn main(){}\n",
            FormatOutcome::Formatted {
                formatter: "rustfmt",
            },
        )
        .await
        .expect("changed content must produce a note");
        assert!(
            note.contains("reformatted"),
            "note must state the file was reformatted: {note}"
        );
        assert!(
            note.contains("fn main() {}"),
            "note must echo the formatted on-disk content: {note}"
        );
    }

    #[tokio::test]
    async fn should_render_no_note_when_formatter_left_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        // Formatter ran but produced byte-identical output: the LLM's mental
        // copy is already accurate — no note.
        let note = note_for_outcome(
            &file,
            "fn main() {}\n",
            FormatOutcome::Formatted {
                formatter: "rustfmt",
            },
        )
        .await;
        assert_eq!(note, None);
    }

    #[tokio::test]
    async fn should_render_failure_note_without_failing_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main(){}\n").unwrap();

        let note = note_for_outcome(
            &file,
            "fn main(){}\n",
            FormatOutcome::Failed {
                formatter: "rustfmt",
                detail: "exit status 1: expected `;`".to_string(),
            },
        )
        .await
        .expect("failure must be surfaced as a note");
        assert!(note.contains("failed"), "note must mention failure: {note}");
        assert!(
            note.contains("edit"),
            "note must reassure the edit was kept: {note}"
        );
    }

    #[tokio::test]
    async fn should_render_timeout_note() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main(){}\n").unwrap();

        let note = note_for_outcome(
            &file,
            "fn main(){}\n",
            FormatOutcome::TimedOut {
                formatter: "rustfmt",
            },
        )
        .await
        .expect("timeout must be surfaced as a note");
        assert!(
            note.contains("timed out"),
            "note must mention the timeout: {note}"
        );
    }

    #[tokio::test]
    async fn should_truncate_formatted_echo_for_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.rs");
        let big = "// filler line to blow past the echo cap\n".repeat(2000);
        std::fs::write(&file, &big).unwrap();

        let note = note_for_outcome(
            &file,
            "fn main(){}\n",
            FormatOutcome::Formatted {
                formatter: "rustfmt",
            },
        )
        .await
        .expect("changed content must produce a note");
        assert!(
            note.len() < big.len(),
            "echo must be capped ({} vs {})",
            note.len(),
            big.len()
        );
        assert!(
            note.contains("truncated"),
            "capped echo must carry a truncation marker: {note}"
        );
    }
}
