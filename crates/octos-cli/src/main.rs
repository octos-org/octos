//! octos CLI entry point.

use clap::Parser;
use color_eyre::eyre::Result;

#[cfg_attr(not(feature = "api"), allow(unused_imports))]
use octos_cli::commands::{self, Args, Executable};

/// Interactive = at least one of stdout/stderr is a TTY. When running as a
/// launchd daemon both are redirected to /dev/null, so this returns false and
/// log init drops the console layer in favour of the rolling file logger —
/// giving the service "one primary logging path" instead of duplicated sinks.
fn is_interactive_terminal() -> bool {
    use std::io::IsTerminal as _;

    std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
}

/// Enable the console tracing layer only when the invocation is interactive
/// (dev/debug) OR when no rolling-file sink is configured (fallback so logs
/// don't vanish entirely).
fn should_enable_console_logs(has_rolling_file_logs: bool, interactive: bool) -> bool {
    !has_rolling_file_logs || interactive
}

fn main() -> Result<()> {
    // Initialize error handling
    color_eyre::install()?;

    // Parse arguments first to determine logging setup
    let args = Args::parse();

    // Determine log directory for serve command (enables rolling file logs)
    #[allow(unused_mut)]
    let mut log_dir: Option<std::path::PathBuf> = None;
    #[cfg(feature = "api")]
    if let commands::Command::Serve(ref cmd) = args.command {
        let data_dir = commands::resolve_data_dir(cmd.data_dir.clone())?;
        let dir = data_dir.join("logs");
        std::fs::create_dir_all(&dir).ok();
        log_dir = Some(dir);
    }

    // Initialize tracing (with optional rolling file output for serve)
    let _log_guard = init_tracing(log_dir.as_deref())?;

    args.command.execute()
}

/// Initialize tracing with console output and optional rolling file output.
///
/// When `log_dir` is `Some`, logs are also written to daily-rotated files
/// under that directory (e.g. `~/.octos/logs/serve.2026-03-09.log`), keeping
/// the last 7 days.  The returned guard must be held for the program lifetime.
fn init_tracing(
    log_dir: Option<&std::path::Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use std::io::IsTerminal as _;
    use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        // Suppress noisy HTML5 parser warnings ("foster parenting not implemented")
        .add_directive("html5ever=error".parse().unwrap());

    // Check if JSON format is requested via environment
    let json_logs = std::env::var("OCTOS_LOG_JSON").is_ok();
    let has_rolling_file_logs = log_dir.is_some();
    let console_enabled =
        should_enable_console_logs(has_rolling_file_logs, is_interactive_terminal());

    // Console layer routing: when the operator has a rolling-file sink AND is
    // not running interactively (is_terminal() == false), suppress stderr to
    // avoid the launchd StandardErrorPath double-capturing what the rolling
    // appender is already persisting. When interactive, keep stderr alive so
    // `octos chat`/debugging still prints.
    let enable_console = console_enabled && (log_dir.is_none() || std::io::stderr().is_terminal());
    let _unused_has_rolling = has_rolling_file_logs; // retained for future use

    if let Some(dir) = log_dir {
        // Rolling daily log file, keep last 7 days
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("serve")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)
            .map_err(|e| eyre::eyre!("failed to create log file appender: {e}"))?;

        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let writer = if enable_console {
            BoxMakeWriter::new(std::io::stderr.and(non_blocking))
        } else {
            BoxMakeWriter::new(non_blocking)
        };

        if json_logs {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(true)
                        .with_current_span(true)
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_ansi(false)
                        .with_target(false)
                        .compact()
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        }

        Ok(Some(guard))
    } else {
        if json_logs {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(true)
                        .with_current_span(true),
                )
                .with(filter)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_target(false)
                        .with_thread_ids(false)
                        .compact(),
                )
                .with(filter)
                .init();
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::should_enable_console_logs;

    #[test]
    fn interactive_tty_with_file_logs_still_gets_console() {
        assert!(should_enable_console_logs(true, true));
    }

    #[test]
    fn daemon_with_file_logs_drops_console() {
        assert!(!should_enable_console_logs(true, false));
    }

    #[test]
    fn daemon_without_file_logs_falls_back_to_console() {
        assert!(should_enable_console_logs(false, false));
    }

    #[test]
    fn interactive_without_file_logs_gets_console() {
        assert!(should_enable_console_logs(false, true));
    }
}
