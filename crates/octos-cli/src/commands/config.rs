//! `octos config`: an interactive wizard + inspection for the layered startup
//! config (`config.json` → the `cli.<cmd>` block).
//!
//! The wizard walks the `serve` / `gateway` / `chat` flags — **introspected from
//! the clap tree** so it stays in sync as flags are added rather than
//! hand-mirrored — explains each, shows its built-in default and the value
//! currently saved, and writes the chosen values back by *merging* (keys the
//! user does not touch survive). The saved values are the startup defaults; an
//! explicit CLI flag or env var still overrides them (see [`crate::config_layer`]).
//! "Skip" (empty input) never writes a value, so a built-in default keeps
//! applying.
//!
//! This is the server half of the config-wizard feature; `octos-tui config` is
//! the client half.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, CommandFactory, Subcommand};
use eyre::{Result, WrapErr, bail, eyre};

use super::Executable;

/// `octos config` — configure octos interactively and inspect the saved config.
#[derive(Debug, Args)]
pub struct ConfigCommand {
    #[command(subcommand)]
    action: Option<ConfigAction>,

    /// Config file to read/write (default: the resolved `config.json`).
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
    /// Walk the serve/gateway/chat flags, explain each, and save your choices
    /// (this is the default).
    Wizard,
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
            None | Some(ConfigAction::Wizard) => self.wizard(&path),
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

    fn wizard(&self, path: &Path) -> Result<()> {
        if !is_tty() {
            bail!(
                "`octos config` needs an interactive terminal. Run it in a terminal, \
                 or edit {} directly.",
                path.display()
            );
        }

        let current = load_current_cli_block(path)?;
        println!("Configuring octos — saves to {}", path.display());
        println!("These become the startup defaults; an explicit CLI flag or env var still wins.");
        println!("Press Enter to skip an option (keep its current or default value).\n");

        // Introspect the clap tree so new flags appear here automatically.
        let top = crate::commands::Args::command();
        let mut collected: BTreeMap<String, serde_json::Map<String, serde_json::Value>> =
            BTreeMap::new();

        for &name in crate::config_layer::LAYERED_COMMANDS {
            let Some(sub_cmd) = top.get_subcommands().find(|cmd| cmd.get_name() == name) else {
                // e.g. `serve` when built without the `api` feature.
                continue;
            };
            let defaults = command_defaults(name).unwrap_or_default();
            let current_section = current
                .get(name)
                .and_then(|value| value.as_object())
                .cloned()
                .unwrap_or_default();

            println!("── {name} ──");
            let mut answers = serde_json::Map::new();
            for arg in sub_cmd.get_arguments() {
                if !crate::config_layer::is_layerable(name, arg) {
                    continue;
                }
                let id = arg.get_id().as_str().to_string();
                let long = arg
                    .get_long()
                    .map(String::from)
                    .unwrap_or_else(|| id.clone());
                let help = arg
                    .get_help()
                    .map(|help| help.to_string())
                    .unwrap_or_default();
                let answer = prompt_value(
                    arg,
                    &long,
                    &help,
                    defaults.get(&id),
                    current_section.get(&id),
                )?;
                if let Some(value) = answer {
                    answers.insert(id, value);
                }
            }
            println!();
            if !answers.is_empty() {
                collected.insert(name.to_string(), answers);
            }
        }

        if collected.is_empty() {
            println!("Nothing changed.");
            return Ok(());
        }

        // Validate every section against its command struct BEFORE touching the
        // file, so a wrong-typed value (bad port, unknown enum) fails loudly
        // rather than producing a config the runtime would reject.
        for (name, answers) in &collected {
            validate_section(name, answers)?;
        }

        crate::config::write_mutation(path, |root| {
            let obj = root
                .as_object_mut()
                .ok_or_else(|| eyre!("{} is not a JSON object", path.display()))?;
            let cli = obj
                .entry("cli")
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            let cli_obj = cli
                .as_object_mut()
                .ok_or_else(|| eyre!("config `cli` block is not a JSON object"))?;
            for (name, answers) in &collected {
                let section = cli_obj
                    .entry(name.clone())
                    .or_insert_with(|| serde_json::Value::Object(Default::default()));
                let section_obj = section
                    .as_object_mut()
                    .ok_or_else(|| eyre!("config `cli.{name}` is not a JSON object"))?;
                for (key, value) in answers {
                    section_obj.insert(key.clone(), value.clone());
                }
            }
            Ok(())
        })?;

        let total: usize = collected.values().map(serde_json::Map::len).sum();
        println!("Saved {total} setting(s) to {}", path.display());
        println!("Run `octos config show` to review.");
        Ok(())
    }
}

fn show(path: &Path) -> Result<()> {
    match std::fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => {
            println!(
                "{} is empty. Run `octos config` to set it up.",
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
                "No config yet at {}.\nRun `octos config` (or `octos init`) to create one.",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).wrap_err_with(|| format!("failed to read {}", path.display())),
    }
}

/// The serialized built-in defaults for a subcommand (field id → default value),
/// used to type the prompts and show `[default: …]`.
fn command_defaults(name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let value = match name {
        #[cfg(feature = "api")]
        "serve" => default_as::<super::ServeCommand>(),
        "gateway" => default_as::<super::GatewayCommand>(),
        "chat" => default_as::<super::ChatCommand>(),
        _ => None,
    };
    value.and_then(|value| value.as_object().cloned())
}

/// Parse a subcommand with an empty argv to capture clap's built-in defaults,
/// then serialize the struct. The command name is irrelevant to default
/// resolution, so a fixed `'static` placeholder is used.
fn default_as<T>() -> Option<serde_json::Value>
where
    T: clap::Args + serde::Serialize,
{
    let cmd = T::augment_args(clap::Command::new("cmd"));
    let matches = cmd.try_get_matches_from(["cmd"]).ok()?;
    let default = T::from_arg_matches(&matches).ok()?;
    serde_json::to_value(&default).ok()
}

/// Validate `answers` for `name` by overlaying them onto the built-in defaults
/// and deserializing into the concrete command struct.
fn validate_section(
    name: &str,
    answers: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    match name {
        #[cfg(feature = "api")]
        "serve" => validate_as::<super::ServeCommand>("serve", answers),
        "gateway" => validate_as::<super::GatewayCommand>("gateway", answers),
        "chat" => validate_as::<super::ChatCommand>("chat", answers),
        _ => Ok(()),
    }
}

fn validate_as<T>(name: &str, answers: &serde_json::Map<String, serde_json::Value>) -> Result<()>
where
    T: clap::Args + serde::Serialize + serde::de::DeserializeOwned,
{
    let mut base =
        default_as::<T>().ok_or_else(|| eyre!("failed to compute defaults for `{name}`"))?;
    let obj = base
        .as_object_mut()
        .ok_or_else(|| eyre!("defaults for `{name}` are not a JSON object"))?;
    for (key, value) in answers {
        obj.insert(key.clone(), value.clone());
    }
    serde_json::from_value::<T>(base)
        .map(|_| ())
        .wrap_err_with(|| format!("the collected `{name}` settings don't match the config schema"))
}

/// Dispatch a single option to the right prompt shape, typed by the arg's action
/// (bool), its possible values (enum), or its default's JSON type (number vs
/// string).
fn prompt_value(
    arg: &clap::Arg,
    long: &str,
    help: &str,
    default_val: Option<&serde_json::Value>,
    current_val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    if matches!(arg.get_action(), clap::ArgAction::SetTrue) {
        return prompt_bool(long, help, default_val, current_val);
    }
    let choices: Vec<String> = arg
        .get_possible_values()
        .iter()
        .map(|value| value.get_name().to_string())
        .collect();
    if !choices.is_empty() {
        return prompt_choice(long, help, &choices, default_val, current_val);
    }
    let numeric = matches!(default_val, Some(serde_json::Value::Number(_)));
    prompt_scalar(long, help, numeric, default_val, current_val)
}

fn prompt_bool(
    long: &str,
    help: &str,
    default_val: Option<&serde_json::Value>,
    current_val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    print_arg_header(long, help, default_val, current_val);
    let raw = read_line("  yes/no, or Enter to skip: ")?;
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "on" | "1" => Some(true.into()),
        "n" | "no" | "false" | "off" | "0" => Some(false.into()),
        _ => None,
    })
}

fn prompt_choice(
    long: &str,
    help: &str,
    choices: &[String],
    default_val: Option<&serde_json::Value>,
    current_val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    print_arg_header(long, help, default_val, current_val);
    for (index, choice) in choices.iter().enumerate() {
        println!("  [{}] {choice}", index + 1);
    }
    let raw = read_line("  choose a number, or Enter to skip: ")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let index: usize = trimmed
        .parse()
        .ok()
        .filter(|n| (1..=choices.len()).contains(n))
        .ok_or_else(|| eyre!("`{trimmed}` is not one of 1..={}", choices.len()))?;
    Ok(Some(choices[index - 1].clone().into()))
}

fn prompt_scalar(
    long: &str,
    help: &str,
    numeric: bool,
    default_val: Option<&serde_json::Value>,
    current_val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    print_arg_header(long, help, default_val, current_val);
    let raw = read_line("  value, or Enter to skip: ")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if numeric {
        let number: i64 = trimmed
            .parse()
            .map_err(|_| eyre!("--{long} expects a number, got `{trimmed}`"))?;
        Ok(Some(number.into()))
    } else {
        Ok(Some(trimmed.to_string().into()))
    }
}

fn print_arg_header(
    long: &str,
    help: &str,
    default_val: Option<&serde_json::Value>,
    current_val: Option<&serde_json::Value>,
) {
    let mut line = format!("--{long}");
    let help = first_line(help);
    if !help.is_empty() {
        line.push_str(" — ");
        line.push_str(help);
    }
    if let Some(value) = default_val.filter(|value| !value.is_null()) {
        line.push_str(&format!("  [default: {}]", render_scalar(value)));
    }
    if let Some(value) = current_val.filter(|value| !value.is_null()) {
        line.push_str(&format!("  [current: {}]", render_scalar(value)));
    }
    println!("{line}");
}

fn render_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Read the existing `cli` block (subcommand → object) for showing current
/// values. Absent/empty file → empty; malformed → surfaced (never overwritten).
fn load_current_cli_block(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    match std::fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => Ok(serde_json::Map::new()),
        Ok(contents) => {
            let value: serde_json::Value = serde_json::from_str(&contents)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
            Ok(value
                .get("cli")
                .and_then(|value| value.as_object())
                .cloned()
                .unwrap_or_default())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(error) => Err(error).wrap_err_with(|| format!("failed to read {}", path.display())),
    }
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .wrap_err("failed to read input")?;
    Ok(line)
}

fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_type_prompts_from_default_json_type() {
        // A numeric default → numeric coercion; a null/absent default → string.
        assert!(matches!(
            serde_json::json!(50080),
            serde_json::Value::Number(_)
        ));
        // The wizard's numeric branch parses "9090" into a JSON number.
        let out = prompt_scalar_for_test(true, "9090").unwrap().unwrap();
        assert_eq!(out, serde_json::json!(9090));
        // The string branch keeps host text verbatim.
        let out = prompt_scalar_for_test(false, "0.0.0.0").unwrap().unwrap();
        assert_eq!(out, serde_json::json!("0.0.0.0"));
        // A non-numeric answer for a numeric field is rejected.
        assert!(prompt_scalar_for_test(true, "notanumber").is_err());
    }

    /// Test shim for the numeric/string coercion in [`prompt_scalar`] without
    /// touching stdin.
    fn prompt_scalar_for_test(numeric: bool, input: &str) -> Result<Option<serde_json::Value>> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if numeric {
            let number: i64 = trimmed
                .parse()
                .map_err(|_| eyre!("expects a number, got `{trimmed}`"))?;
            Ok(Some(number.into()))
        } else {
            Ok(Some(trimmed.to_string().into()))
        }
    }

    #[test]
    fn should_reject_invalid_values_before_writing() {
        // A port outside u16 range must fail validation (caught before write).
        let mut answers = serde_json::Map::new();
        answers.insert("port".into(), serde_json::json!(70000));
        #[cfg(feature = "api")]
        assert!(
            validate_section("serve", &answers).is_err(),
            "out-of-range port must be rejected"
        );

        // A valid gateway flag passes.
        let mut ok = serde_json::Map::new();
        ok.insert("no_retry".into(), serde_json::json!(true));
        assert!(validate_section("gateway", &ok).is_ok());

        // An unknown chat enum value must be rejected.
        let mut bad_enum = serde_json::Map::new();
        bad_enum.insert("sandbox".into(), serde_json::json!("not-a-mode"));
        assert!(
            validate_section("chat", &bad_enum).is_err(),
            "unknown enum value must be rejected"
        );

        // A valid chat enum value (matching clap's kebab possible-value) passes.
        let mut ok_enum = serde_json::Map::new();
        ok_enum.insert("sandbox".into(), serde_json::json!("workspace-write"));
        assert!(validate_section("chat", &ok_enum).is_ok());
    }

    #[test]
    fn should_compute_command_defaults() {
        // gateway/chat are always present; their defaults must serialize.
        let gw = command_defaults("gateway").expect("gateway defaults");
        assert!(gw.contains_key("no_retry"));
        let chat = command_defaults("chat").expect("chat defaults");
        // clap `default_value = "20"` for chat max_iterations round-trips.
        assert_eq!(chat.get("max_iterations"), Some(&serde_json::json!(20)));
    }

    #[test]
    fn should_only_prompt_layerable_flags() {
        // The wizard's include filter is exactly config_layer::is_layerable, so
        // assert the curated in/out decisions on the REAL command tree.
        let top = crate::commands::Args::command();
        let gateway = top
            .get_subcommands()
            .find(|cmd| cmd.get_name() == "gateway")
            .expect("gateway subcommand");
        let prompted: Vec<String> = gateway
            .get_arguments()
            .filter(|arg| crate::config_layer::is_layerable("gateway", arg))
            .filter_map(|arg| arg.get_long().map(String::from))
            .collect();
        assert!(
            prompted.contains(&"no-retry".to_string()),
            "no-retry is prompted"
        );
        assert!(
            !prompted.contains(&"provider".to_string()),
            "provider denied (legacy)"
        );
        assert!(
            !prompted.contains(&"config".to_string()),
            "config denied (selector)"
        );
        assert!(
            !prompted.contains(&"profile".to_string()),
            "gateway profile denied"
        );
        // The hidden managed-gateway internals are never prompted.
        assert!(
            !prompted.contains(&"bridge-url".to_string()),
            "hidden flags denied"
        );
    }

    #[test]
    fn should_render_scalars_without_quoting_strings() {
        assert_eq!(render_scalar(&serde_json::json!("127.0.0.1")), "127.0.0.1");
        assert_eq!(render_scalar(&serde_json::json!(50080)), "50080");
        assert_eq!(render_scalar(&serde_json::json!(true)), "true");
    }

    #[test]
    fn should_round_trip_command_structs_through_serde() {
        // The layering serializes the parsed struct, overlays keys, then
        // deserializes it back — so `to_value` → `from_value` MUST be an
        // identity on the defaults, or a field would be silently lost/altered.
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
