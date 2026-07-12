//! Layered startup config for the `serve` / `gateway` / `chat` subcommands.
//!
//! # Precedence
//!
//! ```text
//! explicit CLI flag  >  env var  >  config.json `cli.<cmd>`  >  built-in default
//! ```
//!
//! clap already resolves *CLI > env > default* into the parsed struct. The gap
//! this module fills is inserting the JSON `cli.<cmd>` block **between env and
//! default**: a field the user did not set on the command line and that did not
//! come from an env var falls back to `config.cli.<cmd>` when present, otherwise
//! keeps clap's built-in default.
//!
//! The "was it explicit?" oracle is [`clap::ArgMatches::value_source`]
//! (`CommandLine` / `EnvVariable` / `DefaultValue`) — the only mechanism that
//! covers both `default_value` scalars (`--port`) and bare `SetTrue` bool flags
//! (`--solo`) without changing a single field type on the large, established
//! command structs.
//!
//! ## Naming convention
//!
//! JSON keys under `cli.<cmd>` are the clap **arg id** (snake_case), which is
//! also the serde field name — so `arg.get_id()`, `value_source(id)`, and the
//! serialized struct key all align on one convention with no rename layer. The
//! wizard displays the kebab-case `--long` name but stores the snake id.
//!
//! ## Fields that are never layered
//!
//! The four legacy fields already dual-sourced from the top-level `Config`
//! (`provider` / `model` / `base_url` / `max_iterations`) are left reading the
//! top level as before — layering them here would double-source them. Config
//! selectors (`config` / `cwd` / `data_dir`), secrets (`auth_token`), one-shot
//! flags (`message`), and the dangerous `--yolo` / `--danger-full-access` flags
//! are excluded too. See [`is_layerable`].

use std::path::PathBuf;

/// Subcommands that participate in the layered startup config.
pub const LAYERED_COMMANDS: &[&str] = &["serve", "gateway", "chat"];

/// Merge `config.cli.<cmd>` defaults into the active subcommand's parsed struct,
/// for any field the user did not set explicitly (CLI) or via an env var.
///
/// Best-effort: a missing/malformed config, or a merged value that fails to
/// deserialize, leaves `args` untouched (the command loads config itself and
/// surfaces any real error). Non-layered commands are a no-op.
pub fn apply(args: &mut crate::commands::Args, matches: &clap::ArgMatches) -> eyre::Result<()> {
    use clap::CommandFactory;

    let Some((name, sub)) = matches.subcommand() else {
        return Ok(());
    };
    if !LAYERED_COMMANDS.contains(&name) {
        return Ok(());
    }
    let Some(section) = load_cli_section(name, sub) else {
        return Ok(());
    };
    if section.is_empty() {
        return Ok(());
    }

    // Rebuild the clap tree for arg introspection (ids, hidden flags). `top`
    // is held for the lifetime of the borrowed `sub_cmd`.
    let top = crate::commands::Args::command();
    let Some(sub_cmd) = top.get_subcommands().find(|cmd| cmd.get_name() == name) else {
        return Ok(());
    };

    match &mut args.command {
        #[cfg(feature = "api")]
        crate::commands::Command::Serve(inner) => overlay(name, inner, sub, sub_cmd, &section),
        crate::commands::Command::Gateway(inner) => overlay(name, inner, sub, sub_cmd, &section),
        crate::commands::Command::Chat(inner) => overlay(name, inner, sub, sub_cmd, &section),
        _ => {}
    }
    Ok(())
}

/// Overlay JSON defaults onto a parsed subcommand struct.
///
/// Generic over any command struct that round-trips through serde: the resolved
/// struct is serialized to a JSON object, each layerable-and-non-explicit field
/// present in `section` is replaced, and the object is deserialized back.
fn overlay<T>(
    cmd: &str,
    typed: &mut T,
    sub_matches: &clap::ArgMatches,
    sub_cmd: &clap::Command,
    section: &serde_json::Map<String, serde_json::Value>,
) where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::to_value(&*typed) else {
        tracing::debug!(
            command = cmd,
            "config.cli overlay skipped: struct not serializable"
        );
        return;
    };

    // A chat invocation carrying an explicit full-access request (`--yolo` or
    // `--sandbox danger-full-access`) must win outright: overlaying a saved
    // `sandbox`/`ask_for_approval` would make `resolve_chat_permissions` abort
    // on contradictory flags (codex).
    let chat_full_access = cmd == "chat" && chat_explicit_full_access(&obj, sub_matches);

    let mut changed = false;
    for arg in sub_cmd.get_arguments() {
        if !is_layerable(cmd, arg) {
            continue;
        }
        let id = arg.get_id().as_str();
        let Some(json_val) = section.get(id) else {
            continue;
        };
        if !should_overlay(sub_matches.value_source(id)) {
            continue;
        }
        if cmd == "chat" && chat_control_blocked(id, json_val, chat_full_access) {
            continue;
        }
        obj.insert(id.to_string(), json_val.clone());
        changed = true;
    }

    if !changed {
        return;
    }
    match serde_json::from_value::<T>(serde_json::Value::Object(obj)) {
        Ok(updated) => *typed = updated,
        Err(error) => {
            tracing::warn!(
                command = cmd,
                %error,
                "ignoring config.cli overlay: merged value failed to deserialize"
            );
        }
    }
}

/// Should the JSON `cli.<cmd>` default overlay a field, given how clap sourced
/// its current value?
///
/// JSON is inserted only when the value did NOT come from an explicit CLI flag
/// or a clap-tracked env var — encoding the precedence
/// `explicit-CLI > env > JSON > default`. A `DefaultValue` or an unset arg
/// (`None`) yields the built-in default, which JSON is allowed to replace.
///
/// (octos does not enable clap's `env` feature — its `OCTOS_*` env vars are read
/// manually in the command bodies, which re-assert env over the layered value
/// for the few flags that honour them, e.g. `serve --solo`. The `EnvVariable`
/// arm here keeps this correct should any arg ever adopt clap `env`.)
fn should_overlay(source: Option<clap::parser::ValueSource>) -> bool {
    !matches!(
        source,
        Some(clap::parser::ValueSource::CommandLine) | Some(clap::parser::ValueSource::EnvVariable)
    )
}

/// Sandbox value that disables the sandbox AND approvals — full access. Never a
/// persistable default: excluded from the wizard and refused by the overlay, so
/// full access stays a per-run opt-in (`--yolo` / explicit `--sandbox`).
pub const DANGER_FULL_ACCESS_SANDBOX: &str = "danger-full-access";

/// Does this `octos chat` invocation carry an EXPLICIT full-access request on
/// the command line — `--yolo` (`dangerously_bypass_approvals_and_sandbox`) or
/// `--sandbox danger-full-access`? Read from the already-parsed struct (`obj`
/// reflects clap's CLI resolution) plus `value_source` to confirm the sandbox
/// value was CLI-supplied, not defaulted.
fn chat_explicit_full_access(
    obj: &serde_json::Map<String, serde_json::Value>,
    sub_matches: &clap::ArgMatches,
) -> bool {
    let yolo = obj
        .get("dangerously_bypass_approvals_and_sandbox")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let explicit_danger_sandbox = matches!(
        sub_matches.value_source("sandbox"),
        Some(clap::parser::ValueSource::CommandLine)
    ) && obj.get("sandbox").and_then(serde_json::Value::as_str)
        == Some(DANGER_FULL_ACCESS_SANDBOX);
    yolo || explicit_danger_sandbox
}

/// Should this saved chat control be REFUSED even though it's otherwise
/// layerable and unset on the CLI?
///
/// 1. A persisted `sandbox: danger-full-access` is never applied — full access
///    is a per-run opt-in (`--yolo` / explicit `--sandbox`), not a saved default
///    that would silently disable the sandbox on every `octos chat`.
/// 2. When the invocation already carries an explicit full-access request, the
///    saved `sandbox` / `ask_for_approval` controls are skipped so the explicit
///    request wins instead of tripping the contradictory-flags check.
fn chat_control_blocked(id: &str, json_val: &serde_json::Value, full_access: bool) -> bool {
    if id == "sandbox" && json_val.as_str() == Some(DANGER_FULL_ACCESS_SANDBOX) {
        return true;
    }
    full_access && matches!(id, "sandbox" | "ask_for_approval")
}

/// Is `arg` safe to persist in `cli.<cmd>` and overlay at startup?
///
/// Shared by the layering (here) and the `octos config` wizard so the two can
/// never disagree about which flags are in scope.
pub fn is_layerable(cmd: &str, arg: &clap::Arg) -> bool {
    if arg.is_hide_set() {
        return false;
    }
    let id = arg.get_id().as_str();
    !is_denied(cmd, id) && !looks_secret(id)
}

/// The static denylist behind [`is_layerable`].
fn is_denied(cmd: &str, id: &str) -> bool {
    // Never layered / persisted, on ANY command:
    //  * config / cwd / data_dir select WHERE config lives (chicken/egg);
    //  * auth_token is a secret → auth store, never JSON;
    //  * provider / model / base_url / max_iterations are the legacy fields
    //    already sourced from the TOP-LEVEL Config — layering them here would
    //    double-source them;
    //  * message is a one-shot (send-and-exit) flag, meaningless as a default;
    //  * the yolo / danger-full-access flags are dangerous — require an
    //    explicit CLI opt-in every run;
    //  * help / version are clap builtins.
    const DENY: &[&str] = &[
        "config",
        "cwd",
        "data_dir",
        "auth_token",
        "provider",
        "model",
        "base_url",
        "max_iterations",
        "message",
        "dangerously_bypass_approvals_and_sandbox",
        "danger_full_access",
        "help",
        "version",
    ];
    if DENY.contains(&id) {
        return true;
    }
    // `octos gateway --profile <FILE>` is a config-file SELECTOR
    // (conflicts_with --config); unlike `octos chat --profile <name>`, which is
    // a runtime-behaviour flag, it must never be persisted.
    if cmd == "gateway" && id == "profile" {
        return true;
    }
    false
}

/// Heuristic secret filter: never persist anything key/token/secret-shaped.
fn looks_secret(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    ["api_key", "apikey", "token", "secret", "password", "passwd"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Load `config.cli.<name>` the SAME way the command will resolve its config.
///
/// The path selectors (`config` / `cwd` / `data_dir`) are read from the CLI
/// only — they are on the denylist, so they can never come from JSON, which
/// means there is no circular dependency. Any error yields `None` ("no
/// layering"); the command re-loads config and reports the real error.
fn load_cli_section(
    name: &str,
    sub: &clap::ArgMatches,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let cwd = sub
        .try_get_one::<PathBuf>("cwd")
        .ok()
        .flatten()
        .cloned()
        .or_else(|| std::env::current_dir().ok())?;
    let data_dir = sub
        .try_get_one::<PathBuf>("data_dir")
        .ok()
        .flatten()
        .cloned();
    let ctx = crate::config_context::resolve_config_context(data_dir.as_deref());
    let config = match sub.try_get_one::<PathBuf>("config").ok().flatten() {
        Some(path) => crate::config::Config::from_file(path).ok()?,
        None => crate::config::Config::load_with_context(&cwd, &ctx).ok()?,
    };
    config
        .cli
        .get(name)
        .and_then(|value| value.as_object())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestCmd {
        port: u16,
        host: String,
        solo: bool,
    }

    /// A stand-in for a serve-like subcommand: a `default_value` scalar, a
    /// `default_value` string, and a bare `SetTrue` bool flag.
    fn test_command() -> Command {
        Command::new("serve")
            .arg(
                Arg::new("port")
                    .long("port")
                    .value_parser(clap::value_parser!(u16))
                    .default_value("50080"),
            )
            .arg(Arg::new("host").long("host").default_value("127.0.0.1"))
            .arg(Arg::new("solo").long("solo").action(ArgAction::SetTrue))
    }

    fn resolve(matches: &clap::ArgMatches) -> TestCmd {
        TestCmd {
            port: *matches.get_one::<u16>("port").unwrap(),
            host: matches.get_one::<String>("host").unwrap().clone(),
            solo: matches.get_flag("solo"),
        }
    }

    fn section(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn should_prefer_explicit_cli_over_json() {
        let cmd = test_command();
        let matches = cmd.get_matches_from(["serve", "--port", "9090"]);
        let mut typed = resolve(&matches);

        overlay(
            "serve",
            &mut typed,
            &matches,
            &test_command(),
            &section(serde_json::json!({ "port": 8080 })),
        );

        assert_eq!(typed.port, 9090, "explicit --port must beat JSON");
    }

    #[test]
    fn should_use_json_when_flag_absent() {
        let cmd = test_command();
        let matches = cmd.get_matches_from(["serve"]);
        let mut typed = resolve(&matches);
        assert_eq!(typed.port, 50080, "sanity: clap default before overlay");

        overlay(
            "serve",
            &mut typed,
            &matches,
            &test_command(),
            &section(serde_json::json!({ "port": 8080, "solo": true })),
        );

        assert_eq!(typed.port, 8080, "JSON must beat the built-in default");
        assert!(typed.solo, "JSON must set an unset SetTrue bool flag");
    }

    #[test]
    fn should_keep_default_when_json_absent() {
        let cmd = test_command();
        let matches = cmd.get_matches_from(["serve"]);
        let mut typed = resolve(&matches);

        overlay(
            "serve",
            &mut typed,
            &matches,
            &test_command(),
            &section(serde_json::json!({ "host": "0.0.0.0" })),
        );

        assert_eq!(
            typed.port, 50080,
            "unmentioned key keeps its built-in default"
        );
        assert_eq!(typed.host, "0.0.0.0", "mentioned key comes from JSON");
    }

    #[test]
    fn should_encode_precedence_in_should_overlay() {
        use clap::parser::ValueSource;
        // explicit CLI flag and (clap-tracked) env var both outrank JSON.
        assert!(
            !should_overlay(Some(ValueSource::CommandLine)),
            "CLI beats JSON"
        );
        assert!(
            !should_overlay(Some(ValueSource::EnvVariable)),
            "env beats JSON"
        );
        // default / unset are overridable by JSON.
        assert!(
            should_overlay(Some(ValueSource::DefaultValue)),
            "JSON beats default"
        );
        assert!(should_overlay(None), "JSON fills an unset arg");
    }

    #[test]
    fn should_never_layer_a_persisted_full_access_sandbox() {
        let danger = serde_json::Value::from(DANGER_FULL_ACCESS_SANDBOX);
        // Refused whether or not the run requested full access — a saved
        // danger-full-access must never silently disable the sandbox.
        assert!(chat_control_blocked("sandbox", &danger, false));
        assert!(chat_control_blocked("sandbox", &danger, true));
        // A safe saved sandbox / approval is fine absent an explicit request.
        assert!(!chat_control_blocked(
            "sandbox",
            &serde_json::Value::from("workspace-write"),
            false
        ));
        assert!(!chat_control_blocked(
            "ask_for_approval",
            &serde_json::Value::from("ask"),
            false
        ));
    }

    #[test]
    fn should_skip_saved_controls_when_explicit_full_access() {
        // An explicit --yolo / --sandbox danger-full-access must win: the saved
        // sandbox + approval controls are skipped so they can't trip the
        // contradictory-flags error in resolve_chat_permissions.
        assert!(chat_control_blocked(
            "sandbox",
            &serde_json::Value::from("workspace-write"),
            true
        ));
        assert!(chat_control_blocked(
            "ask_for_approval",
            &serde_json::Value::from("ask"),
            true
        ));
        // Unrelated saved controls still layer under full access.
        assert!(!chat_control_blocked(
            "verbose",
            &serde_json::Value::Bool(true),
            true
        ));
    }

    #[test]
    fn should_detect_explicit_chat_full_access() {
        use clap::{Arg, ArgAction, Command};
        let cmd = Command::new("chat")
            .arg(
                Arg::new("dangerously_bypass_approvals_and_sandbox")
                    .long("yolo")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("sandbox").long("sandbox").action(ArgAction::Set));

        // --yolo → full access.
        let matches = cmd.clone().get_matches_from(["chat", "--yolo"]);
        let mut obj = serde_json::Map::new();
        obj.insert(
            "dangerously_bypass_approvals_and_sandbox".into(),
            true.into(),
        );
        assert!(chat_explicit_full_access(&obj, &matches));

        // --sandbox danger-full-access (CLI-sourced) → full access.
        let matches =
            cmd.clone()
                .get_matches_from(["chat", "--sandbox", DANGER_FULL_ACCESS_SANDBOX]);
        let mut obj = serde_json::Map::new();
        obj.insert("sandbox".into(), DANGER_FULL_ACCESS_SANDBOX.into());
        assert!(chat_explicit_full_access(&obj, &matches));

        // Neither → not full access.
        let matches = cmd.get_matches_from(["chat"]);
        assert!(!chat_explicit_full_access(
            &serde_json::Map::new(),
            &matches
        ));
    }

    #[test]
    fn should_deny_layering_of_sensitive_and_selector_flags() {
        let provider = Arg::new("provider").long("provider");
        let auth = Arg::new("auth_token").long("auth-token");
        let cfg = Arg::new("config").long("config");
        let hidden = Arg::new("bridge_url").long("bridge-url").hide(true);
        let port = Arg::new("port").long("port");
        let gw_profile = Arg::new("profile").long("profile");

        assert!(
            !is_layerable("serve", &provider),
            "provider is legacy top-level"
        );
        assert!(!is_layerable("serve", &auth), "auth_token is a secret");
        assert!(
            !is_layerable("serve", &cfg),
            "config selects the config file"
        );
        assert!(
            !is_layerable("gateway", &hidden),
            "hidden flags are internal"
        );
        assert!(
            !is_layerable("gateway", &gw_profile),
            "gateway --profile selects config"
        );
        assert!(
            is_layerable("serve", &port),
            "port is a normal layerable flag"
        );
    }

    #[test]
    fn should_allow_chat_profile_but_deny_gateway_profile() {
        let profile = Arg::new("profile").long("profile");
        assert!(
            is_layerable("chat", &profile),
            "chat --profile is a runtime flag"
        );
        assert!(
            !is_layerable("gateway", &profile),
            "gateway --profile is a selector"
        );
    }

    #[test]
    fn should_flag_secret_shaped_ids() {
        assert!(looks_secret("auth_token"));
        assert!(looks_secret("openai_api_key"));
        assert!(looks_secret("webhook_secret"));
        assert!(!looks_secret("port"));
        assert!(!looks_secret("host"));
    }
}
