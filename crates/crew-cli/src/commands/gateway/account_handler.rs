//! /account command handler for sub-account management.

use std::sync::Arc;

/// Verify a sub-account belongs to the given parent, returning the profile or an error message.
fn verify_sub_account(
    store: &crate::profiles::ProfileStore,
    sub_id: &str,
    parent_id: &str,
) -> Result<crate::profiles::UserProfile, String> {
    match store.get(sub_id) {
        Ok(Some(p)) if p.parent_id.as_deref() == Some(parent_id) => Ok(p),
        Ok(Some(_)) => Err(format!("'{sub_id}' is not a sub-account of this profile.")),
        Ok(None) => Err(format!("Sub-account '{sub_id}' not found.")),
        Err(e) => Err(format!("Error: {e}")),
    }
}

/// Handle /account command — sub-account CRUD operations.
pub async fn handle_account_command(
    args: &str,
    parent_profile_id: Option<&str>,
    profile_store: &Option<Arc<crate::profiles::ProfileStore>>,
) -> String {
    let parent_id = match parent_profile_id {
        Some(id) => id,
        None => return "Account management requires a profile-based gateway.".to_string(),
    };

    let store = match profile_store {
        Some(s) => s,
        None => {
            return "Account management is not available (no crew-home configured).".to_string();
        }
    };

    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    match parts.first().copied().unwrap_or("list") {
        "" | "list" => match store.list_sub_accounts(parent_id) {
            Ok(subs) if subs.is_empty() => {
                "No sub-accounts.\nCreate one with: /account create <name>".to_string()
            }
            Ok(subs) => {
                let mut lines = vec!["Sub-accounts:".to_string()];
                for s in &subs {
                    let status = if s.enabled { "enabled" } else { "disabled" };
                    let ch_types: Vec<&str> = s
                        .config
                        .channels
                        .iter()
                        .map(|c| match c {
                            crate::profiles::ChannelCredentials::Telegram { .. } => "telegram",
                            crate::profiles::ChannelCredentials::Discord { .. } => "discord",
                            crate::profiles::ChannelCredentials::Slack { .. } => "slack",
                            crate::profiles::ChannelCredentials::WhatsApp { .. } => "whatsapp",
                            crate::profiles::ChannelCredentials::Feishu { .. } => "feishu",
                            crate::profiles::ChannelCredentials::Email { .. } => "email",
                            crate::profiles::ChannelCredentials::Twilio { .. } => "twilio",
                            crate::profiles::ChannelCredentials::Api { .. } => "api",
                            crate::profiles::ChannelCredentials::WeComBot { .. } => "wecom-bot",
                        })
                        .collect();
                    let sb = if s.config.sandbox.enabled {
                        " | sandbox: ON"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "  {} — {} ({}) [{}]{sb}",
                        s.id,
                        s.name,
                        status,
                        ch_types.join(", ")
                    ));
                }
                lines.join("\n")
            }
            Err(e) => format!("Error: {e}"),
        },

        "create" => {
            let name = parts.get(1).copied().unwrap_or("").trim();
            if name.is_empty() {
                return "Usage: /account create <name>".to_string();
            }
            match store.create_sub_account(
                parent_id,
                name,
                vec![],
                crate::profiles::GatewaySettings::default(),
            ) {
                Ok(sub) => format!(
                    "Created sub-account: {}\nAdd channels via dashboard or CLI:\n  crew account create --profile {} {} --telegram-token <token>",
                    sub.id, parent_id, name
                ),
                Err(e) => format!("Error: {e}"),
            }
        }

        "delete" => {
            let sub_id = parts.get(1).copied().unwrap_or("").trim();
            if sub_id.is_empty() {
                return "Usage: /account delete <sub-id>".to_string();
            }
            if let Err(msg) = verify_sub_account(store, sub_id, parent_id) {
                return msg;
            }
            match store.delete(sub_id) {
                Ok(true) => format!("Deleted sub-account: {sub_id}"),
                Ok(false) => format!("Sub-account '{sub_id}' not found"),
                Err(e) => format!("Error: {e}"),
            }
        }

        "update" => handle_account_update(parts.get(1).copied().unwrap_or(""), parent_id, store),

        // /account start <sub-id> — enable and trigger gateway start
        "start" | "enable" => {
            let sub_id = parts.get(1).copied().unwrap_or("").trim();
            if sub_id.is_empty() {
                return "Usage: /account start <sub-id>".to_string();
            }
            let mut profile = match verify_sub_account(store, sub_id, parent_id) {
                Ok(p) => p,
                Err(msg) => return msg,
            };
            profile.enabled = true;
            profile.updated_at = chrono::Utc::now();
            match store.save(&profile) {
                Ok(()) => {
                    format!("Enabled sub-account: {sub_id}\nGateway will start within ~5 seconds.")
                }
                Err(e) => format!("Error saving: {e}"),
            }
        }

        // /account stop <sub-id> — disable and trigger gateway stop
        "stop" | "disable" => {
            let sub_id = parts.get(1).copied().unwrap_or("").trim();
            if sub_id.is_empty() {
                return "Usage: /account stop <sub-id>".to_string();
            }
            let mut profile = match verify_sub_account(store, sub_id, parent_id) {
                Ok(p) => p,
                Err(msg) => return msg,
            };
            profile.enabled = false;
            profile.updated_at = chrono::Utc::now();
            match store.save(&profile) {
                Ok(()) => {
                    format!("Disabled sub-account: {sub_id}\nGateway will stop within ~5 seconds.")
                }
                Err(e) => format!("Error saving: {e}"),
            }
        }

        // /account restart <sub-id> — touch profile to trigger gateway restart
        "restart" => {
            let sub_id = parts.get(1).copied().unwrap_or("").trim();
            if sub_id.is_empty() {
                return "Usage: /account restart <sub-id>".to_string();
            }
            let mut profile = match verify_sub_account(store, sub_id, parent_id) {
                Ok(p) => p,
                Err(msg) => return msg,
            };
            if !profile.enabled {
                return format!(
                    "Sub-account '{sub_id}' is disabled. Use /account start {sub_id} first."
                );
            }
            profile.updated_at = chrono::Utc::now();
            match store.save(&profile) {
                Ok(()) => format!(
                    "Restarting sub-account: {sub_id}\nGateway will restart within ~5 seconds."
                ),
                Err(e) => format!("Error saving: {e}"),
            }
        }

        other => format!(
            "Unknown sub-command: {other}\nUsage: /account [list|create|update|delete|start|stop|restart]\n  /account list — list sub-accounts\n  /account create <name> — create sub-account\n  /account update <sub-id> key=value ... — update config\n  /account start <sub-id> — enable & start gateway\n  /account stop <sub-id> — disable & stop gateway\n  /account restart <sub-id> — restart gateway\n  /account delete <sub-id> — delete sub-account"
        ),
    }
}

/// Handle /account update <sub-id> key=value key=value ...
fn handle_account_update(
    rest: &str,
    parent_id: &str,
    store: &crate::profiles::ProfileStore,
) -> String {
    let rest = rest.trim();
    let mut tokens = rest.splitn(2, ' ');
    let sub_id = tokens.next().unwrap_or("").trim();
    let kv_str = tokens.next().unwrap_or("").trim();
    if sub_id.is_empty() || kv_str.is_empty() {
        return "Usage: /account update <sub-id> key=value [key=value ...]\n\
            Keys: telegram-token, telegram-senders, whatsapp, \
            feishu-app-id, feishu-app-secret, system-prompt, enabled, \
            sandbox, sandbox-mode, sandbox-network\n\
            Example: /account update my--bot sandbox=true sandbox-mode=auto"
            .to_string();
    }
    let mut profile = match verify_sub_account(store, sub_id, parent_id) {
        Ok(p) => p,
        Err(msg) => return msg,
    };

    let mut changed = Vec::new();

    // Parse key=value pairs (simple split on '=')
    for pair in kv_str.split_whitespace() {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let val = kv.next().unwrap_or("");
        match key {
            "telegram-token" => {
                let env_name = format!(
                    "TELEGRAM_BOT_TOKEN_{}",
                    profile.name.to_uppercase().replace([' ', '-'], "_")
                );
                profile.config.channels.retain(|ch| {
                    !matches!(ch, crate::profiles::ChannelCredentials::Telegram { .. })
                });
                profile
                    .config
                    .channels
                    .push(crate::profiles::ChannelCredentials::Telegram {
                        token_env: env_name.clone(),
                        allowed_senders: String::new(),
                    });
                profile.config.env_vars.insert(env_name, val.to_string());
                changed.push("telegram channel");
            }
            "telegram-senders" => {
                let mut found = false;
                for ch in &mut profile.config.channels {
                    if let crate::profiles::ChannelCredentials::Telegram {
                        allowed_senders, ..
                    } = ch
                    {
                        *allowed_senders = val.to_string();
                        found = true;
                    }
                }
                if found {
                    changed.push("telegram senders");
                } else {
                    return "No Telegram channel to update senders on. Set telegram-token first."
                        .to_string();
                }
            }
            "whatsapp" => {
                profile.config.channels.retain(|ch| {
                    !matches!(ch, crate::profiles::ChannelCredentials::WhatsApp { .. })
                });
                if val == "true" || val == "1" {
                    profile
                        .config
                        .channels
                        .push(crate::profiles::ChannelCredentials::WhatsApp {
                            bridge_url: String::new(),
                        });
                    changed.push("whatsapp enabled");
                } else {
                    changed.push("whatsapp disabled");
                }
            }
            "feishu-app-id" | "feishu-app-secret" => {
                let id_env = format!(
                    "LARK_APP_ID_{}",
                    profile.name.to_uppercase().replace([' ', '-'], "_")
                );
                let secret_env = format!(
                    "LARK_APP_SECRET_{}",
                    profile.name.to_uppercase().replace([' ', '-'], "_")
                );
                if key == "feishu-app-id" {
                    profile
                        .config
                        .env_vars
                        .insert(id_env.clone(), val.to_string());
                } else {
                    profile
                        .config
                        .env_vars
                        .insert(secret_env.clone(), val.to_string());
                }
                // Ensure Feishu channel exists
                if !profile
                    .config
                    .channels
                    .iter()
                    .any(|ch| matches!(ch, crate::profiles::ChannelCredentials::Feishu { .. }))
                {
                    profile
                        .config
                        .channels
                        .push(crate::profiles::ChannelCredentials::Feishu {
                            app_id_env: id_env,
                            app_secret_env: secret_env,
                            mode: "webhook".to_string(),
                            region: String::new(),
                            webhook_port: None,
                            verification_token_env: String::new(),
                            encrypt_key_env: String::new(),
                        });
                }
                changed.push("feishu channel");
            }
            "system-prompt" => {
                profile.config.gateway.system_prompt = if val.is_empty() {
                    None
                } else {
                    Some(val.to_string())
                };
                changed.push("system prompt");
            }
            "enabled" => {
                let en = val == "true" || val == "1";
                profile.enabled = en;
                changed.push(if en { "enabled" } else { "disabled" });
            }
            "sandbox" => {
                let on = val == "true" || val == "1" || val == "on";
                profile.config.sandbox.enabled = on;
                changed.push(if on {
                    "sandbox enabled"
                } else {
                    "sandbox disabled"
                });
            }
            "sandbox-mode" => {
                profile.config.sandbox.mode = match val {
                    "auto" => crew_agent::SandboxMode::Auto,
                    "macos" => crew_agent::SandboxMode::Macos,
                    "docker" => crew_agent::SandboxMode::Docker,
                    "bwrap" => crew_agent::SandboxMode::Bwrap,
                    _ => {
                        return format!(
                            "Invalid sandbox mode: {val}\nValid modes: auto, macos, docker, bwrap"
                        );
                    }
                };
                changed.push("sandbox mode");
            }
            "sandbox-network" => {
                let net = val == "true" || val == "1";
                profile.config.sandbox.allow_network = net;
                changed.push(if net {
                    "sandbox network allowed"
                } else {
                    "sandbox network blocked"
                });
            }
            _ => {
                return format!(
                    "Unknown key: {key}\nValid keys: telegram-token, telegram-senders, whatsapp, feishu-app-id, feishu-app-secret, system-prompt, enabled, sandbox, sandbox-mode, sandbox-network"
                );
            }
        }
    }

    if changed.is_empty() {
        return "Nothing to update.".to_string();
    }

    profile.updated_at = chrono::Utc::now();
    match store.save(&profile) {
        Ok(()) => {
            let mut msg = format!("Updated sub-account: {sub_id}");
            for c in &changed {
                msg.push_str(&format!("\n  - {c}"));
            }
            msg.push_str("\nGateway will auto-restart to pick up changes.");
            msg
        }
        Err(e) => format!("Error saving: {e}"),
    }
}
