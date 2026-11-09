//! `octos config`: read-only inspection of the layered startup config
//! (`config.json` → the `cli.<cmd>` block).
//!
//! The interactive wizard was removed — operators hand-edit `config.json`
//! directly. `octos config show` prints the saved config and `octos config
//! path` prints the resolved `config.json` path; bare `octos config` prints a
//! short overview pointing at both plus the file location and an "edit it
//! directly" hint (it does NOT launch a wizard).
//!
//! The layered `cli.<cmd>` startup config (explicit CLI flag › env var › JSON ›
//! built-in default) is unchanged — see [`crate::config_layer`]. The block is
//! now hand-edited in `config.json` rather than wizard-written; an explicit CLI
//! flag or env var still overrides it.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use eyre::{Result, WrapErr};

use super::Executable;

/// `octos config` — inspect the saved startup config (read-only).
#[derive(Debug, Args)]
pub struct ConfigCommand {
    #[command(subcommand)]
    action: Option<ConfigAction>,

    /// Config file to read (default: the resolved `config.json`).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Data directory (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Working directory (for project-local config resolution).
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the saved config.json.
    Show,
    /// Print the resolved config.json path.
    Path,
}

impl Executable for ConfigCommand {
    fn execute(self) -> Result<()> {
        let path = self.resolve_path()?;
        match self.action {
            Some(ConfigAction::Path) => {
                println!("{}", path.display());
                Ok(())
            }
            Some(ConfigAction::Show) => show(&path),
            // Bare `octos config`: print a read-only overview, never a wizard.
            None => {
                print!("{}", overview(&path));
                Ok(())
            }
        }
    }
}

impl ConfigCommand {
    /// Resolve the target `config.json` the same way the runtime does.
    fn resolve_path(&self) -> Result<PathBuf> {
        let cwd = self
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let ctx = crate::config_context::resolve_config_context(self.data_dir.as_deref());
        Ok(crate::config::resolve_config_file_path(
            &cwd,
            &ctx,
            self.config.as_deref(),
        ))
    }
}

/// The bare-`octos config` overview: what the command does now (read-only
/// inspection), where the config file lives, and how to change settings (edit
/// the file directly). Kept as a pure function so it stays trivially testable.
fn overview(path: &Path) -> String {
    let path = path.display();
    // `serve` is only compiled with the `api` feature, so a `cli.serve` block is
    // inert in a non-api build — list only the commands actually available so we
    // don't point operators at ineffective config (codex).
    let layered = if cfg!(feature = "api") {
        "serve/gateway/chat"
    } else {
        "gateway/chat"
    };
    format!(
        "octos config — inspect the saved startup config (read-only).\n\
         \n\
         Config file: {path}\n\
         \n\
         Commands:\n  \
         octos config show   Print the saved config.json.\n  \
         octos config path   Print the resolved config.json path.\n\
         \n\
         To change settings, edit {path} directly (or run `octos init` for a\n\
         provider quickstart). The `cli.<cmd>` block sets startup defaults for\n\
         {layered}; an explicit CLI flag or env var still wins.\n"
    )
}

fn show(path: &Path) -> Result<()> {
    match std::fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => {
            println!(
                "{} is empty. Edit it directly (or run `octos init`) to set it up.",
                path.display()
            );
            Ok(())
        }
        Ok(contents) => {
            println!("# {}", path.display());
            println!("{}", contents.trim_end());
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "No config yet at {}.\nEdit it directly (or run `octos init`) to create one.",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).wrap_err_with(|| format!("failed to read {}", path.display())),
    }
}
