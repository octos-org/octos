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
         serve/gateway/chat; an explicit CLI flag or env var still wins.\n"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_point_bare_config_at_show_path_and_file_without_a_wizard() {
        // Bare `octos config` prints a read-only overview: it names the
        // `show` / `path` subcommands, shows the resolved config location, and
        // tells the user to edit the file directly. It must NOT reference a
        // wizard — that interactive flow was removed.
        let path = Path::new("/tmp/octos/config.json");
        let text = overview(path);
        assert!(text.contains("show"), "names the show subcommand");
        assert!(text.contains("path"), "names the path subcommand");
        assert!(
            text.contains("/tmp/octos/config.json"),
            "shows the resolved config path"
        );
        assert!(
            text.to_ascii_lowercase().contains("edit"),
            "hints to edit the file directly"
        );
        assert!(
            !text.to_ascii_lowercase().contains("wizard"),
            "the interactive wizard flow is gone"
        );
    }

    #[test]
    fn should_round_trip_command_structs_through_serde() {
        // The layered `cli.<cmd>` startup config serializes the parsed struct,
        // overlays keys, then deserializes it back (see `crate::config_layer`)
        // — so `to_value` → `from_value` MUST be an identity on the defaults,
        // or a field would be silently lost/altered. Guarded here because these
        // are the exact structs the overlay round-trips.
        fn assert_round_trip<T>()
        where
            T: clap::Args
                + serde::Serialize
                + serde::de::DeserializeOwned
                + PartialEq
                + std::fmt::Debug,
        {
            let cmd = T::augment_args(clap::Command::new("cmd"));
            let matches = cmd.try_get_matches_from(["cmd"]).unwrap();
            let original = T::from_arg_matches(&matches).unwrap();
            let value = serde_json::to_value(&original).unwrap();
            let restored: T = serde_json::from_value(value).unwrap();
            assert_eq!(
                original, restored,
                "command struct must round-trip via serde"
            );
        }

        #[cfg(feature = "api")]
        assert_round_trip::<crate::commands::ServeCommand>();
        assert_round_trip::<crate::commands::GatewayCommand>();
        assert_round_trip::<crate::commands::ChatCommand>();
    }
}
