//! Auth command: login, logout, status, and keychain management.

use std::io::Write as _;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::Result;

use super::Executable;
use crate::auth::{AuthStore, keychain, oauth, token};
use crate::profiles::ProfileStore;

/// Manage authentication for LLM providers.
#[derive(Debug, Args)]
pub struct AuthCommand {
    #[command(subcommand)]
    pub action: AuthAction,
}

#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Log in to an LLM provider.
    Login {
        /// Provider name (openai, anthropic, gemini, etc.).
        #[arg(long, short)]
        provider: String,

        /// Use device code flow instead of browser (OpenAI only).
        #[arg(long)]
        device_code: bool,
    },
    /// Log out from a provider.
    Logout {
        /// Provider name.
        #[arg(long, short)]
        provider: String,
    },
    /// Show authentication status for all providers.
    Status,

    /// Store an API key in the macOS Keychain.
    #[command(name = "set-key")]
    SetKey {
        /// Environment variable name (e.g. OPENAI_API_KEY).
        name: String,
        /// The secret value. If omitted, reads interactively.
        value: Option<String>,
        /// Profile ID to update. If omitted, updates all profiles that have this key.
        #[arg(long, short)]
        profile: Option<String>,
    },
    /// List API keys and their storage status (keychain vs plaintext).
    #[command(name = "keys")]
    Keys {
        /// Profile ID to check. If omitted, shows keys from all profiles.
        #[arg(long, short)]
        profile: Option<String>,
    },
    /// Remove an API key from the macOS Keychain.
    #[command(name = "remove-key")]
    RemoveKey {
        /// Environment variable name to remove (e.g. OPENAI_API_KEY).
        name: String,
        /// Profile ID to update. If omitted, updates all profiles.
        #[arg(long, short)]
        profile: Option<String>,
    },

    /// Unlock the macOS Keychain for SSH sessions.
    ///
    /// Required before set-key/remove-key when connected via SSH.
    /// With auto-login enabled, this is only needed once per boot.
    #[command(name = "unlock")]
    Unlock {
        /// macOS login password. If omitted, reads interactively.
        #[arg(long)]
        password: Option<String>,
    },
}

impl Executable for AuthCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(self.run_async())
    }
}

impl AuthCommand {
    async fn run_async(self) -> Result<()> {
        match self.action {
            AuthAction::Login {
                provider,
                device_code,
            } => login(&provider, device_code).await,
            AuthAction::Logout { provider } => logout(&provider),
            AuthAction::Status => status(),
            AuthAction::SetKey {
                name,
                value,
                profile,
            } => set_key(&name, value, profile.as_deref()),
            AuthAction::Keys { profile } => list_keys(profile.as_deref()),
            AuthAction::RemoveKey { name, profile } => remove_key(&name, profile.as_deref()),
            AuthAction::Unlock { password } => unlock_keychain(password),
        }
    }
}

async fn login(provider: &str, device_code: bool) -> Result<()> {
    let cred = match provider {
        "openai" => {
            if device_code {
                oauth::device_code_flow().await?
            } else {
                oauth::browser_oauth_flow().await?
            }
        }
        // All other providers use paste-token flow.
        _ => token::paste_token_flow(provider)?,
    };

    let mut store = AuthStore::load()?;
    store.set(provider, cred)?;

    println!(
        "{} Logged in to {} (credentials saved)",
        "OK".green().bold(),
        provider
    );
    Ok(())
}

fn logout(provider: &str) -> Result<()> {
    let mut store = AuthStore::load()?;
    if store.remove(provider)? {
        println!("{} Logged out from {}", "OK".green().bold(), provider);
    } else {
        println!("No credentials found for {provider}");
    }
    Ok(())
}

fn status() -> Result<()> {
    let store = AuthStore::load()?;
    let creds: Vec<_> = store.list().collect();

    if creds.is_empty() {
        println!(
            "No saved credentials. Use {} to log in.",
            "crew auth login".cyan()
        );
        return Ok(());
    }

    println!("{}", "Authenticated providers:".bold());
    for (name, cred) in creds {
        let method = &cred.auth_method;
        let status = if cred.is_expired() {
            "expired".red().to_string()
        } else {
            "active".green().to_string()
        };
        let expiry = cred
            .expires_at
            .map(|t| format!(" (expires {})", t.format("%Y-%m-%d %H:%M UTC")))
            .unwrap_or_default();
        println!("  {name}: {status} [{method}]{expiry}");
    }
    Ok(())
}

// ── Keychain subcommands ───────────────────────────────────────────────────

fn open_profile_store() -> Result<ProfileStore> {
    let home = dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
    ProfileStore::open(&home.join(".crew"))
}

fn set_key(name: &str, value: Option<String>, profile_id: Option<&str>) -> Result<()> {
    // Get the secret value: from argument or interactive prompt
    let secret = match value {
        Some(v) => v,
        None => {
            print!("Enter value for {}: ", name.cyan());
            std::io::stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() {
                eyre::bail!("no value provided");
            }
            trimmed
        }
    };

    // Store in keychain
    keychain::set_secret(name, &secret)?;

    // Update profile(s) to use keychain marker
    let store = open_profile_store()?;
    let profiles = get_profiles(&store, profile_id)?;

    let mut updated_count = 0;
    for mut profile in profiles {
        if profile.config.env_vars.contains_key(name) {
            profile
                .config
                .env_vars
                .insert(name.to_string(), keychain::KEYCHAIN_MARKER.to_string());
            profile.updated_at = chrono::Utc::now();
            store.save(&profile)?;
            updated_count += 1;
            println!(
                "  {} profile '{}' updated to use keychain",
                "->".dimmed(),
                profile.id.cyan()
            );
        }
    }

    println!(
        "{} Stored {} in keychain ({})",
        "OK".green().bold(),
        name.cyan(),
        if updated_count > 0 {
            format!("{updated_count} profile(s) updated")
        } else {
            "no profiles reference this key".to_string()
        }
    );
    Ok(())
}

fn list_keys(profile_id: Option<&str>) -> Result<()> {
    let store = open_profile_store()?;
    let profiles = get_profiles(&store, profile_id)?;

    // Collect all unique env var names and their storage type
    let mut keychain_keys = std::collections::BTreeSet::new();
    let mut plain_keys = std::collections::BTreeSet::new();

    for profile in &profiles {
        for (key, value) in &profile.config.env_vars {
            if value == keychain::KEYCHAIN_MARKER {
                keychain_keys.insert(key.clone());
            } else if !value.is_empty() {
                plain_keys.insert(key.clone());
            }
        }
    }

    if keychain_keys.is_empty() && plain_keys.is_empty() {
        println!("No API keys configured in any profile.");
        return Ok(());
    }

    if !keychain_keys.is_empty() {
        println!("{}", "Keychain-stored keys:".bold());
        for key in &keychain_keys {
            let status = match keychain::get_secret(key) {
                Ok(Some(_)) => "available".green().to_string(),
                Ok(None) => "missing from keychain!".red().to_string(),
                Err(_) => "keychain error".yellow().to_string(),
            };
            println!("  {key}: {status}");
        }
    }

    if !plain_keys.is_empty() {
        if !keychain_keys.is_empty() {
            println!();
        }
        println!("{}", "Plaintext keys (in profile JSON):".bold());
        for key in &plain_keys {
            println!("  {key}: {}", "plaintext".dimmed());
        }
    }

    Ok(())
}

fn remove_key(name: &str, profile_id: Option<&str>) -> Result<()> {
    // Remove from keychain
    let deleted = keychain::delete_secret(name)?;

    // Remove env_var entry from profile(s) that use keychain marker
    let store = open_profile_store()?;
    let profiles = get_profiles(&store, profile_id)?;

    let mut updated_count = 0;
    for mut profile in profiles {
        if profile.config.env_vars.get(name).map(|v| v.as_str()) == Some(keychain::KEYCHAIN_MARKER)
        {
            profile.config.env_vars.remove(name);
            profile.updated_at = chrono::Utc::now();
            store.save(&profile)?;
            updated_count += 1;
        }
    }

    if deleted {
        println!(
            "{} Removed {} from keychain ({} profile(s) updated)",
            "OK".green().bold(),
            name.cyan(),
            updated_count
        );
    } else {
        println!(
            "No keychain entry found for {} ({} profile(s) updated)",
            name, updated_count
        );
    }
    Ok(())
}

fn unlock_keychain(password: Option<String>) -> Result<()> {
    // Check if already accessible
    if keychain::is_accessible() {
        println!("{} Keychain is already unlocked", "OK".green().bold());
        return Ok(());
    }

    let pw = match password {
        Some(p) => p,
        None => {
            print!("macOS login password: ");
            std::io::stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim().to_string()
        }
    };

    keychain::unlock(&pw)?;

    println!(
        "{} Keychain unlocked (auto-lock disabled)",
        "OK".green().bold()
    );
    Ok(())
}

/// Get profiles matching the optional filter, or all profiles.
fn get_profiles(
    store: &ProfileStore,
    profile_id: Option<&str>,
) -> Result<Vec<crate::profiles::UserProfile>> {
    if let Some(id) = profile_id {
        match store.get(id)? {
            Some(p) => Ok(vec![p]),
            None => eyre::bail!("profile '{id}' not found"),
        }
    } else {
        store.list()
    }
}
