//! Profile command: portable profile export/import surfaces.
//!
//! `octos profile qr` renders the resolved local configuration as a
//! scannable QR (`OCTOS1:` plain / `OCTOS1E:` PIN-encrypted — see
//! [`crate::profile_qr`]) so a mobile client can onboard by pointing a
//! camera at the terminal. `octos profile decode` is the inverse,
//! for verifying a payload or importing one by hand.

use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr, bail};

use super::Executable;
use crate::config::Config;
use crate::profile_qr::{self, ProfileQrPayload};

/// Portable profile export (QR) and payload inspection.
#[derive(Debug, Args)]
pub struct ProfileCommand {
    #[command(subcommand)]
    action: ProfileAction,
}

#[derive(Debug, Subcommand)]
enum ProfileAction {
    /// Render the resolved local config as a scannable profile QR.
    Qr {
        /// Include resolved provider API keys in the payload. Forces the
        /// PIN-encrypted OCTOS1E format unless --plain-secrets is given.
        #[arg(long)]
        include_secrets: bool,
        /// DANGEROUS with --include-secrets: emit secrets in the plain
        /// OCTOS1 format (anyone who photographs the QR owns the keys).
        #[arg(long, requires = "include_secrets")]
        plain_secrets: bool,
        /// PIN for the encrypted format (default: auto-generated 6 digits,
        /// printed beside the QR).
        #[arg(long)]
        pin: Option<String>,
        /// Serve endpoint the scanning client should connect to
        /// (e.g. https://ada.crew.example.com).
        #[arg(long)]
        endpoint: Option<String>,
        /// Profile id embedded in the payload.
        #[arg(long, default_value = "local")]
        id: String,
        /// Human-readable profile name embedded in the payload.
        #[arg(long)]
        name: Option<String>,
        /// Also write the encoded payload string to this file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print only the encoded payload (no QR art, no summary).
        #[arg(long)]
        payload_only: bool,
    },
    /// Decode an OCTOS1/OCTOS1E payload and print the profile JSON.
    Decode {
        /// The payload string ("-" to read from stdin).
        payload: String,
        /// PIN for OCTOS1E payloads.
        #[arg(long)]
        pin: Option<String>,
        /// Print secret values instead of masking them.
        #[arg(long)]
        show_secrets: bool,
    },
}

impl Executable for ProfileCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            ProfileAction::Qr {
                include_secrets,
                plain_secrets,
                pin,
                endpoint,
                id,
                name,
                out,
                payload_only,
            } => run_qr(QrOptions {
                include_secrets,
                plain_secrets,
                pin,
                endpoint,
                id,
                name,
                out,
                payload_only,
            }),
            ProfileAction::Decode {
                payload,
                pin,
                show_secrets,
            } => run_decode(&payload, pin.as_deref(), show_secrets),
        }
    }
}

struct QrOptions {
    include_secrets: bool,
    plain_secrets: bool,
    pin: Option<String>,
    endpoint: Option<String>,
    id: String,
    name: Option<String>,
    out: Option<PathBuf>,
    payload_only: bool,
}

/// Build the wire payload from a resolved local [`Config`].
///
/// Secrets are only resolved (and only for the env vars the config
/// actually references) when `include_secrets` is set — minimal
/// disclosure, nothing speculative.
pub(crate) fn payload_from_config(
    config: &Config,
    id: &str,
    include_secrets: bool,
) -> Result<ProfileQrPayload> {
    let mut payload = ProfileQrPayload::new(id);

    let provider = config.provider.as_deref();
    if let Some(provider) = provider {
        // Emit the SAME structured contract as the server-profile export
        // (`LlmProfileConfig`): one shape for scanning clients (codex P2).
        let mut primary = serde_json::Map::new();
        primary.insert("family_id".into(), provider.into());
        if let Some(ref model) = config.model {
            primary.insert("model_id".into(), model.as_str().into());
        }
        if let Some(ref var) = config.api_key_env {
            primary.insert("route".into(), serde_json::json!({ "api_key_env": var }));
        }
        payload.llm = Some(serde_json::json!({ "primary": primary }));
    }
    if let Some(ref memory) = config.memory {
        payload.memory = Some(serde_json::to_value(memory).wrap_err("serialize memory config")?);
    }
    if let Some(ref embedding) = config.embedding {
        payload.embedding =
            Some(serde_json::to_value(embedding).wrap_err("serialize embedding config")?);
    }
    payload.voice_default = config.voice.as_ref().map(|v| v.default_voice.clone());

    if include_secrets {
        if let Some(provider) = provider {
            let var = config
                .api_key_env
                .clone()
                .or_else(|| Config::provider_default_env_var(provider));
            if let Some(var) = var {
                if let Some(key) = resolve_export_secret(config, &var) {
                    payload.secrets.insert(var, key);
                }
            }
        }
        if let Some(ref emb) = config.embedding {
            let var = emb
                .api_key_env
                .clone()
                .or_else(|| Config::provider_default_env_var(&emb.provider));
            if let Some(var) = var {
                if let Some(key) = resolve_export_secret(config, &var) {
                    payload.secrets.entry(var).or_insert(key);
                }
            }
        }
    }

    Ok(payload)
}

/// Resolve a secret for QR export from the CONFIG-LOCAL map only
/// (`env_vars` + the user's own keychain markers). Deliberately NOT
/// `Config::get_api_key`: that chain consults the global auth store and
/// the process environment, so an export would quietly embed host-level
/// credentials (`octos auth login` tokens, ambient CI vars) that the
/// config file never declared (codex P2).
fn resolve_export_secret(config: &Config, var: &str) -> Option<String> {
    let raw = config.env_vars.get(var)?;
    crate::auth::keychain::resolve_value(var, raw).filter(|value| !value.is_empty())
}

fn run_qr(opts: QrOptions) -> Result<()> {
    let cwd = std::env::current_dir().wrap_err("resolve current directory")?;
    let ctx = crate::config_context::resolve_config_context(None);
    // The loader returns defaults when no config exists, so an Err here is
    // a REAL problem (unreadable/corrupt config) — surfacing it beats
    // silently exporting an empty default profile (codex P3).
    let config = Config::load_with_context(&cwd, &ctx).wrap_err("load config")?;

    if config.provider.is_none() {
        eprintln!(
            "{}",
            "warning: no LLM provider configured — the QR will carry an empty profile".yellow()
        );
    }

    let mut payload = payload_from_config(&config, &opts.id, opts.include_secrets)?;
    payload.name = opts.name;
    payload.endpoint = opts.endpoint;

    // Secrets ride encrypted unless the caller explicitly opted out.
    let (encoded, pin) = if payload.has_secrets() && !opts.plain_secrets {
        let pin = match opts.pin {
            Some(pin) => pin,
            None => profile_qr::generate_pin(),
        };
        (profile_qr::encode_encrypted(&payload, &pin)?, Some(pin))
    } else {
        (
            profile_qr::encode_plain(&payload, opts.plain_secrets)?,
            None,
        )
    };

    if let Some(ref out) = opts.out {
        std::fs::write(out, &encoded).wrap_err_with(|| format!("write {}", out.display()))?;
    }

    if opts.payload_only {
        println!("{encoded}");
        if let Some(pin) = pin {
            eprintln!("PIN: {pin}");
        }
        return Ok(());
    }

    println!("{}", profile_qr::render_terminal(&encoded)?);
    println!("{}", encoded.dimmed());
    println!();
    if payload.has_secrets() && pin.is_none() {
        println!(
            "{}",
            "⚠ secrets are embedded UNENCRYPTED — treat this QR like the keys themselves"
                .red()
                .bold()
        );
    }
    if let Some(pin) = pin {
        println!(
            "Scan with the octos mobile app, then enter PIN: {}",
            pin.bold()
        );
        println!("{}", "(share the PIN separately from the QR)".dimmed());
    } else {
        println!("Scan with the octos mobile app to import this profile.");
    }
    Ok(())
}

fn run_decode(payload: &str, pin: Option<&str>, show_secrets: bool) -> Result<()> {
    let raw = if payload == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .wrap_err("read payload from stdin")?;
        buf
    } else {
        payload.to_string()
    };
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty payload");
    }

    let mut decoded = crate::profile_qr::decode(raw, pin)?;
    if !show_secrets {
        for value in decoded.secrets.values_mut() {
            *value = mask(value);
        }
        if let Some(ref mut token) = decoded.auth_token {
            *token = mask(token);
        }
    }
    println!("{}", serde_json::to_string_pretty(&decoded)?);
    Ok(())
}

/// Mask a secret: keep a short identifying prefix, hide the rest.
fn mask(value: &str) -> String {
    let prefix: String = value.chars().take(6).collect();
    if value.chars().count() <= 6 {
        "•".repeat(value.chars().count())
    } else {
        format!("{prefix}…")
    }
}
