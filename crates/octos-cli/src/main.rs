//! octos CLI entry point.

use clap::Parser;
use color_eyre::eyre::Result;

#[cfg(feature = "api")]
pub mod api;
pub mod auth;
mod commands;
pub mod compaction;
pub mod config;
pub mod config_watcher;
#[cfg(feature = "api")]
pub mod content_catalog;
pub mod cron_tool;
pub mod gateway_dispatcher;
#[cfg(feature = "api")]
pub mod monitor;
#[cfg(feature = "api")]
pub mod otp;
pub mod persona_service;
#[cfg(feature = "api")]
pub mod process_manager;
pub mod profiles;
pub mod project_templates;
pub mod session_actor;
pub mod skills_scope;
pub mod soul_service;
pub mod status_indicator;
pub mod status_layers;
pub mod stream_reporter;
pub mod tenant;
pub mod tools;
#[cfg(feature = "api")]
pub mod updater;
#[cfg(feature = "api")]
pub mod user_store;

use commands::{Args, Executable};

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
    use tracing_subscriber::{fmt, prelude::*, EnvFilter, Layer};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        // Suppress noisy HTML5 parser warnings ("foster parenting not implemented")
        .add_directive("html5ever=error".parse().unwrap());

    // Check if JSON format is requested via environment
    let json_logs = std::env::var("OCTOS_LOG_JSON").is_ok();

    // Console layer (boxed so we can unify json vs compact types)
    let console_layer: Box<dyn Layer<_> + Send + Sync> = if json_logs {
        fmt::layer()
            .json()
            .with_target(true)
            .with_span_list(true)
            .with_current_span(true)
            .boxed()
    } else {
        fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .compact()
            .boxed()
    };

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

        let file_layer = fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .compact()
            .with_writer(non_blocking);

        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .with(filter)
            .init();

        Ok(Some(guard))
    } else {
        tracing_subscriber::registry()
            .with(console_layer)
            .with(filter)
            .init();

        Ok(None)
    }
}
