//! octos CLI entry point.

use clap::Parser;
use color_eyre::eyre::Result;

#[cfg_attr(not(feature = "api"), allow(unused_imports))]
use octos_cli::commands::{self, Args, Executable};

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

    let enable_console = log_dir.is_none() || std::io::stderr().is_terminal();

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
