//! Sub-account management commands.
//!
//! Sub-accounts inherit LLM provider config from a parent profile but have
//! their own data directory (memory, sessions, episodes, skills) and channels.

use clap::{Args, Subcommand};
use eyre::{Result, bail};

use super::Executable;
use crate::profiles::{ChannelCredentials, GatewaySettings, ProfileStore};

/// Manage sub-accounts under profiles.
#[derive(Debug, Args)]
pub struct AccountCommand {
    #[command(subcommand)]
    pub action: AccountAction,
}

#[derive(Debug, Subcommand)]
pub enum AccountAction {
    /// List sub-accounts for a profile.
    List {
        /// Parent profile ID.
        #[arg(long)]
        profile: String,
    },
    /// Create a new sub-account under a profile.
    Create {
        /// Parent profile ID.
        #[arg(long)]
        profile: String,
        /// Sub-account display name.
        name: String,
        /// Telegram bot token (creates a Telegram channel).
        #[arg(long)]
        telegram_token: Option<String>,
        /// Enable WhatsApp channel (auto-managed bridge).
        #[arg(long)]
        whatsapp: bool,
        /// System prompt for this sub-account.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Auto-enable after creation.
        #[arg(long)]
        enable: bool,
    },
    /// Update a sub-account (add/change channels, system prompt, enable/disable).
    Update {
        /// Sub-account ID (e.g. parent--work).
        id: String,
        /// Set Telegram bot token (adds or replaces Telegram channel).
        #[arg(long)]
        telegram_token: Option<String>,
        /// Set Telegram allowed sender IDs (comma-separated).
        #[arg(long)]
        telegram_senders: Option<String>,
        /// Enable WhatsApp channel (auto-managed bridge).
        #[arg(long)]
        whatsapp: Option<bool>,
        /// Set Feishu/Lark app ID.
        #[arg(long)]
        feishu_app_id: Option<String>,
        /// Set Feishu/Lark app secret.
        #[arg(long)]
        feishu_app_secret: Option<String>,
        /// Set system prompt.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Enable or disable the sub-account.
        #[arg(long)]
        enabled: Option<bool>,
    },
    /// Delete a sub-account.
    Delete {
        /// Sub-account ID (e.g. parent--work).
        id: String,
    },
    /// Show sub-account info.
    Info {
        /// Sub-account ID.
        id: String,
    },
    /// Enable (start) a sub-account's gateway.
    Start {
        /// Sub-account ID.
        id: String,
    },
    /// Disable (stop) a sub-account's gateway.
    Stop {
        /// Sub-account ID.
        id: String,
    },
    /// Restart a sub-account's gateway (touch profile to trigger watcher).
    Restart {
        /// Sub-account ID.
        id: String,
    },
}

impl Executable for AccountCommand {
    fn execute(self) -> Result<()> {
        let data_dir = super::resolve_data_dir(None)?;
        let store = ProfileStore::open(&data_dir)?;

        match self.action {
            AccountAction::List { profile } => {
                // Verify parent exists
                if store.get(&profile)?.is_none() {
                    bail!("profile '{profile}' not found");
                }

                let subs = store.list_sub_accounts(&profile)?;
                if subs.is_empty() {
                    println!("No sub-accounts for profile '{profile}'.");
                    println!("Create one with: octos account create --profile {profile} <name>");
                    return Ok(());
                }

                println!("Sub-accounts for '{profile}':");
                println!("{:<30} {:<20} {:<10}", "ID", "NAME", "ENABLED");
                println!("{}", "-".repeat(60));
                for s in &subs {
                    let channels: Vec<&str> = s.config.channels.iter().map(channel_type).collect();
                    println!(
                        "{:<30} {:<20} {:<10} [{}]",
                        s.id,
                        s.name,
                        if s.enabled { "yes" } else { "no" },
                        channels.join(", ")
                    );
                }
            }

            AccountAction::Create {
                profile,
                name,
                telegram_token,
                whatsapp,
                system_prompt,
                enable,
            } => {
                let mut channels = Vec::new();
                let mut env_vars = std::collections::HashMap::new();

                if let Some(ref token) = telegram_token {
                    let env_name = format!(
                        "TELEGRAM_BOT_TOKEN_{}",
                        name.to_uppercase().replace(' ', "_")
                    );
                    channels.push(ChannelCredentials::Telegram {
                        token_env: env_name.clone(),
                        allowed_senders: String::new(),
                    });
                    env_vars.insert(env_name, token.clone());
                }

                if whatsapp {
                    channels.push(ChannelCredentials::WhatsApp {
                        bridge_url: String::new(), // auto-managed
                    });
                }

                let gateway = GatewaySettings {
                    system_prompt,
                    ..Default::default()
                };

                let mut sub = store.create_sub_account(&profile, &name, channels, gateway)?;

                // Save env vars
                if !env_vars.is_empty() {
                    sub.config.env_vars = env_vars;
                    if enable {
                        sub.enabled = true;
                    }
                    sub.updated_at = chrono::Utc::now();
                    store.save(&sub)?;
                } else if enable {
                    sub.enabled = true;
                    sub.updated_at = chrono::Utc::now();
                    store.save(&sub)?;
                }

                println!("Created sub-account: {}", sub.id);
                println!("  Name: {}", sub.name);
                println!("  Parent: {profile}");
                println!("  Enabled: {}", sub.enabled);
                println!("  Data dir: {}", store.resolve_data_dir(&sub).display());

                if sub.config.channels.is_empty() {
                    println!();
                    println!("No channels configured. Add via dashboard or:");
                    println!(
                        "  octos account create --profile {profile} <name> --telegram-token <token>"
                    );
                }
            }

            AccountAction::Update {
                id,
                telegram_token,
                telegram_senders,
                whatsapp,
                feishu_app_id,
                feishu_app_secret,
                system_prompt,
                enabled,
            } => {
                let mut profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                if profile.parent_id.is_none() {
                    bail!("'{id}' is a top-level profile. Use the dashboard to update it.");
                }

                let mut changed = Vec::new();

                // Update Telegram channel
                if let Some(ref token) = telegram_token {
                    let env_name = format!(
                        "TELEGRAM_BOT_TOKEN_{}",
                        profile.name.to_uppercase().replace([' ', '-'], "_")
                    );
                    // Remove existing Telegram channel if any
                    profile
                        .config
                        .channels
                        .retain(|ch| !matches!(ch, ChannelCredentials::Telegram { .. }));
                    let senders = telegram_senders.clone().unwrap_or_default();
                    profile.config.channels.push(ChannelCredentials::Telegram {
                        token_env: env_name.clone(),
                        allowed_senders: senders,
                    });
                    profile.config.env_vars.insert(env_name, token.clone());
                    changed.push("telegram channel");
                } else if let Some(ref senders) = telegram_senders {
                    // Update allowed_senders on existing Telegram channel
                    let mut found = false;
                    for ch in &mut profile.config.channels {
                        if let ChannelCredentials::Telegram {
                            allowed_senders, ..
                        } = ch
                        {
                            *allowed_senders = senders.clone();
                            found = true;
                        }
                    }
                    if found {
                        changed.push("telegram senders");
                    } else {
                        bail!(
                            "no Telegram channel to update senders on. Add --telegram-token first."
                        );
                    }
                }

                // Update WhatsApp channel
                if let Some(enable_wa) = whatsapp {
                    profile
                        .config
                        .channels
                        .retain(|ch| !matches!(ch, ChannelCredentials::WhatsApp { .. }));
                    if enable_wa {
                        profile.config.channels.push(ChannelCredentials::WhatsApp {
                            bridge_url: String::new(),
                        });
                        changed.push("whatsapp enabled");
                    } else {
                        changed.push("whatsapp disabled");
                    }
                }

                // Update Feishu channel
                if feishu_app_id.is_some() || feishu_app_secret.is_some() {
                    let app_id = feishu_app_id.unwrap_or_default();
                    let app_secret = feishu_app_secret.unwrap_or_default();

                    let id_env = format!(
                        "LARK_APP_ID_{}",
                        profile.name.to_uppercase().replace([' ', '-'], "_")
                    );
                    let secret_env = format!(
                        "LARK_APP_SECRET_{}",
                        profile.name.to_uppercase().replace([' ', '-'], "_")
                    );

                    // Remove existing Feishu channel if any
                    profile
                        .config
                        .channels
                        .retain(|ch| !matches!(ch, ChannelCredentials::Feishu { .. }));
                    profile.config.channels.push(ChannelCredentials::Feishu {
                        app_id_env: id_env.clone(),
                        app_secret_env: secret_env.clone(),
                        mode: "webhook".to_string(),
                        region: String::new(),
                        webhook_port: None,
                        verification_token_env: String::new(),
                        encrypt_key_env: String::new(),
                    });

                    if !app_id.is_empty() {
                        profile.config.env_vars.insert(id_env, app_id);
                    }
                    if !app_secret.is_empty() {
                        profile.config.env_vars.insert(secret_env, app_secret);
                    }
                    changed.push("feishu channel");
                }

                // Update system prompt
                if let Some(ref prompt) = system_prompt {
                    profile.config.gateway.system_prompt = if prompt.is_empty() {
                        None
                    } else {
                        Some(prompt.clone())
                    };
                    changed.push("system prompt");
                }

                // Update enabled state
                if let Some(en) = enabled {
                    profile.enabled = en;
                    changed.push(if en { "enabled" } else { "disabled" });
                }

                if changed.is_empty() {
                    println!(
                        "Nothing to update. Use flags like --telegram-token, --enabled, --system-prompt."
                    );
                    return Ok(());
                }

                profile.updated_at = chrono::Utc::now();
                store.save(&profile)?;

                println!("Updated sub-account: {}", profile.id);
                for c in &changed {
                    println!("  - {c}");
                }
                println!("\nThe gateway will auto-restart to pick up changes.");
            }

            AccountAction::Delete { id } => {
                let profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                if profile.parent_id.is_none() {
                    bail!(
                        "'{id}' is a top-level profile, not a sub-account. Use the dashboard to delete it."
                    );
                }

                store.delete(&id)?;
                println!("Deleted sub-account: {id}");
            }

            AccountAction::Info { id } => {
                let profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                let parent_label = profile
                    .parent_id
                    .as_deref()
                    .unwrap_or("(none — top-level profile)");

                println!("Sub-account: {}", profile.id);
                println!("  Name: {}", profile.name);
                println!("  Parent: {parent_label}");
                println!("  Enabled: {}", profile.enabled);
                println!("  Data dir: {}", store.resolve_data_dir(&profile).display());

                if !profile.config.channels.is_empty() {
                    println!("  Channels:");
                    for ch in &profile.config.channels {
                        println!("    - {}", channel_type(ch));
                    }
                }

                // Show effective provider if it's a sub-account
                if profile.parent_id.is_some() {
                    match crate::profiles::resolve_effective_profile(&store, &profile) {
                        Ok(eff) => {
                            println!(
                                "  Provider: {} (inherited)",
                                eff.config.provider.as_deref().unwrap_or("none")
                            );
                            println!(
                                "  Model: {} (inherited)",
                                eff.config.model.as_deref().unwrap_or("none")
                            );
                        }
                        Err(e) => {
                            println!("  Provider: error resolving — {e}");
                        }
                    }
                }
            }

            AccountAction::Start { id } => {
                let mut profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                if profile.parent_id.is_none() {
                    bail!("'{id}' is a top-level profile. Manage it via the dashboard.");
                }

                if profile.enabled {
                    println!("Sub-account '{id}' is already enabled.");
                    return Ok(());
                }

                profile.enabled = true;
                profile.updated_at = chrono::Utc::now();
                store.save(&profile)?;
                println!("Enabled sub-account: {id}");
                println!("Gateway will start within ~5 seconds (if octos serve is running).");
            }

            AccountAction::Stop { id } => {
                let mut profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                if profile.parent_id.is_none() {
                    bail!("'{id}' is a top-level profile. Manage it via the dashboard.");
                }

                if !profile.enabled {
                    println!("Sub-account '{id}' is already disabled.");
                    return Ok(());
                }

                profile.enabled = false;
                profile.updated_at = chrono::Utc::now();
                store.save(&profile)?;
                println!("Disabled sub-account: {id}");
                println!("Gateway will stop within ~5 seconds (if octos serve is running).");
            }

            AccountAction::Restart { id } => {
                let mut profile = store
                    .get(&id)?
                    .ok_or_else(|| eyre::eyre!("sub-account '{id}' not found"))?;

                if profile.parent_id.is_none() {
                    bail!("'{id}' is a top-level profile. Manage it via the dashboard.");
                }

                if !profile.enabled {
                    bail!("Sub-account '{id}' is disabled. Use `octos account start {id}` first.");
                }

                // Touch updated_at to trigger the file watcher
                profile.updated_at = chrono::Utc::now();
                store.save(&profile)?;
                println!("Restarting sub-account: {id}");
                println!("Gateway will restart within ~5 seconds (if octos serve is running).");
            }
        }

        Ok(())
    }
}

fn channel_type(ch: &ChannelCredentials) -> &'static str {
    match ch {
        ChannelCredentials::Telegram { .. } => "telegram",
        ChannelCredentials::Discord { .. } => "discord",
        ChannelCredentials::Slack { .. } => "slack",
        ChannelCredentials::WhatsApp { .. } => "whatsapp",
        ChannelCredentials::Feishu { .. } => "feishu",
        ChannelCredentials::Email { .. } => "email",
        ChannelCredentials::Twilio { .. } => "twilio",
        ChannelCredentials::Api { .. } => "api",
        ChannelCredentials::WeComBot { .. } => "wecom-bot",
        ChannelCredentials::Matrix { .. } => "matrix",
        ChannelCredentials::QQBot { .. } => "qq-bot",
        ChannelCredentials::WeChat { .. } => "wechat",
    }
}
