//! Auth command: login, logout, status, and keychain management.

use std::io::Write as _;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::Result;

use super::Executable;
use crate::auth::{AuthStore, keychain, oauth, token};
use crate::config_context::{resolve_config_context, run_migrations};
use crate::profiles::ProfileStore;

/// Open the global auth store for the `auth login/logout/status` commands.
///
/// Auth is GLOBAL: it lives under the resolver's `auth_home` (OCTOS_CONFIG_DIR
/// if set, else the XDG default), independent of `--data-dir`. We run the
/// migrations first so a legacy `~/.octos/auth.json` is copied into the XDG
/// location (0600, legacy left intact) before the store opens.
fn open_global_auth_store() -> Result<AuthStore> {
    let ctx = resolve_config_context(None);
    run_migrations(&ctx);
    AuthStore::open(&ctx)
}

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

    let mut store = open_global_auth_store()?;
    store.set(provider, cred)?;

    println!(
        "{} Logged in to {} (credentials saved)",
        "OK".green().bold(),
        provider
    );
    Ok(())
}

fn logout(provider: &str) -> Result<()> {
    let mut store = open_global_auth_store()?;
    if store.remove(provider)? {
        println!("{} Logged out from {}", "OK".green().bold(), provider);
    } else {
        println!("No credentials found for {provider}");
    }
    Ok(())
}

fn status() -> Result<()> {
    let store = open_global_auth_store()?;
    let creds: Vec<_> = store.list().collect();

    if creds.is_empty() {
        println!(
            "No saved credentials. Use {} to log in.",
            "octos auth login".cyan()
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
    ProfileStore::open(&home.join(".octos"))
}

/// Whether storing `secret` under `name` must be scoped per profile: a declared
/// keychain-backed var, OR any value that is a service-account JSON (detected by
/// content, so the same protection applies under a custom env name such as the
/// dashboard "Custom" provider's `VERTEX_API_KEY`). Mirrors the API-side
/// `keychain::needs_keychain_relocation` contract.
fn should_scope(name: &str, secret: &str) -> bool {
    keychain::KEYCHAIN_BACKED_ENV_VARS.contains(&name) || keychain::is_service_account_json(secret)
}

/// The keychain account and the profile-config marker to persist for `name` in
/// `profile_id`. Scoped creds get a per-profile account + scoped marker; other
/// keys keep the bare account + bare marker.
fn keychain_target(name: &str, profile_id: &str, secret: &str) -> (String, String) {
    if should_scope(name, secret) {
        let account = keychain::scoped_account(name, profile_id);
        let marker = keychain::marker_for(&account);
        (account, marker)
    } else {
        (name.to_string(), keychain::KEYCHAIN_MARKER.to_string())
    }
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

    // Keychain-backed credentials (e.g. a Vertex SA JSON, even under a custom
    // env name) are stored under a PER-PROFILE account so distinct profiles
    // never share — and overwrite — one keychain item. Other keys keep a single
    // shared account.
    let scoped = should_scope(name, &secret);

    // Shared keys: store once up front (also covers the orphan / no-profile
    // case). Scoped keys are stored per profile in the loop below.
    if !scoped {
        keychain::set_secret(name, &secret)?;
    }

    // Update profile(s) to use the keychain marker
    let store = open_profile_store()?;
    let profiles = get_profiles(&store, profile_id)?;

    let mut updated_count = 0;
    for mut profile in profiles {
        if profile.config.env_vars.contains_key(name) {
            let (account, marker) = keychain_target(name, &profile.id, &secret);
            if scoped {
                keychain::set_secret(&account, &secret)?;
            }
            profile.config.env_vars.insert(name.to_string(), marker);
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

    // A scoped key is only ever written inside the per-profile loop, so if no
    // profile referenced it nothing was stored — don't claim otherwise.
    if scoped && updated_count == 0 {
        println!(
            "{} No profile references {}; nothing stored. Add the key to a profile \
             (or save it via the dashboard) first.",
            "!".yellow().bold(),
            name.cyan(),
        );
        return Ok(());
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

    // Collect DISTINCT (env var, keychain account) pairs — the account is
    // resolved from the marker, so a profile-scoped key has one entry per
    // account and we never collapse two profiles' distinct accounts into one
    // (which would hide a missing/broken entry for one of them).
    let mut keychain_keys: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    let mut plain_keys = std::collections::BTreeSet::new();

    for profile in &profiles {
        for (key, value) in &profile.config.env_vars {
            if keychain::is_marker(value) {
                keychain_keys.insert((
                    key.clone(),
                    keychain::marker_account(value, key).to_string(),
                ));
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
        for (key, account) in &keychain_keys {
            let status = match keychain::get_secret(account) {
                Ok(Some(_)) => "available".green().to_string(),
                Ok(None) => "missing from keychain!".red().to_string(),
                Err(_) => "keychain error".yellow().to_string(),
            };
            // Show the scoped account when it differs from the bare env-var name.
            if account == key {
                println!("  {key}: {status}");
            } else {
                println!("  {key} ({account}): {status}");
            }
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

/// What a `remove-key` invocation should do, computed purely (no keychain or
/// store I/O) so it's unit-testable.
struct RemovalPlan {
    /// Profile ids to drop the env var from.
    profiles_to_update: Vec<String>,
    /// Keychain accounts that are safe to delete.
    accounts_to_delete: Vec<String>,
}

/// Plan a `remove-key name` over `entries` — `(profile_id, stored value for
/// name)` for EVERY profile that has the key. `is_target` selects the profiles
/// being removed from.
///
/// The subtle case: a **bare** account (`VERTEX_SA_JSON`) can be shared by many
/// legacy profiles, so it is only deleted when no *non-target* profile still
/// references it. A **scoped** account (`VERTEX_SA_JSON::<id>`) is unique to one
/// profile and always safe to delete.
fn plan_removal(
    entries: &[(String, String)],
    name: &str,
    is_target: impl Fn(&str) -> bool,
) -> RemovalPlan {
    let bare_shared_by_other = entries.iter().any(|(pid, value)| {
        !is_target(pid)
            && keychain::is_marker(value)
            && keychain::marker_account(value, name) == name
    });

    let mut profiles_to_update = Vec::new();
    let mut accounts_to_delete = Vec::new();
    for (pid, value) in entries {
        if !is_target(pid) || !keychain::is_marker(value) {
            continue;
        }
        profiles_to_update.push(pid.clone());
        let account = keychain::marker_account(value, name);
        let shared_bare = account == name && bare_shared_by_other;
        if !shared_bare {
            accounts_to_delete.push(account.to_string());
        }
    }
    accounts_to_delete.sort();
    accounts_to_delete.dedup();
    RemovalPlan {
        profiles_to_update,
        accounts_to_delete,
    }
}

fn remove_key(name: &str, profile_id: Option<&str>) -> Result<()> {
    let store = open_profile_store()?;
    // Load ALL profiles (not just the target) so we can tell whether a shared
    // bare keychain account is still referenced by a profile we're NOT removing.
    let all_profiles = store.list()?;
    if let Some(id) = profile_id {
        if !all_profiles.iter().any(|p| p.id == id) {
            eyre::bail!("profile '{id}' not found");
        }
    }

    let entries: Vec<(String, String)> = all_profiles
        .iter()
        .filter_map(|p| {
            p.config
                .env_vars
                .get(name)
                .map(|v| (p.id.clone(), v.clone()))
        })
        .collect();
    let plan = plan_removal(&entries, name, |pid| match profile_id {
        Some(id) => pid == id,
        None => true,
    });

    let mut deleted = false;
    for account in &plan.accounts_to_delete {
        deleted |= keychain::delete_secret(account).unwrap_or(false);
    }
    // A global removal (no `--profile`) also cleans up a legacy/orphan bare
    // account not referenced by any profile.
    if profile_id.is_none() {
        deleted |= keychain::delete_secret(name).unwrap_or(false);
    }

    let to_update: std::collections::HashSet<&str> =
        plan.profiles_to_update.iter().map(String::as_str).collect();
    let mut updated_count = 0;
    for mut profile in all_profiles {
        if to_update.contains(profile.id.as_str()) {
            profile.config.env_vars.remove(name);
            profile.updated_at = chrono::Utc::now();
            store.save(&profile)?;
            updated_count += 1;
        }
    }

    if updated_count == 0 && !deleted {
        println!("No {} key found to remove.", name.cyan());
    } else {
        // Distinguish "deleted the keychain account" from "removed the env var
        // but kept a shared account another profile still uses".
        let keychain_note = if deleted {
            "keychain account removed".to_string()
        } else {
            "shared keychain account kept (still used by another profile)".to_string()
        };
        println!(
            "{} Removed {} from {} profile(s); {}",
            "OK".green().bold(),
            name.cyan(),
            updated_count,
            keychain_note
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_target_scopes_by_name_and_by_content() {
        let sa = r#"{"type":"service_account","private_key":"x"}"#;
        // Declared keychain-backed name → per-profile scoped account.
        let (account, marker) = keychain_target("VERTEX_SA_JSON", "alice", sa);
        assert_eq!(account, "VERTEX_SA_JSON::alice");
        assert_eq!(marker, "keychain:VERTEX_SA_JSON::alice");

        // SA JSON under a CUSTOM env name (the dashboard "Custom" bypass) is
        // ALSO scoped — by content — so CLI saves can't collide cross-profile.
        let (account, marker) = keychain_target("VERTEX_API_KEY", "alice", sa);
        assert_eq!(account, "VERTEX_API_KEY::alice");
        assert_eq!(marker, "keychain:VERTEX_API_KEY::alice");
        assert_ne!(
            keychain_target("VERTEX_API_KEY", "alice", sa).0,
            keychain_target("VERTEX_API_KEY", "bob", sa).0,
            "distinct profiles must get distinct accounts"
        );

        // An ordinary (non-SA) key keeps the single bare account + bare marker.
        let (account, marker) = keychain_target("OPENAI_API_KEY", "alice", "sk-plain");
        assert_eq!(account, "OPENAI_API_KEY");
        assert_eq!(marker, keychain::KEYCHAIN_MARKER);
    }

    const NAME: &str = "VERTEX_SA_JSON";

    fn entry(pid: &str, value: &str) -> (String, String) {
        (pid.to_string(), value.to_string())
    }

    #[test]
    fn remove_plan_keeps_shared_bare_account_when_another_profile_uses_it() {
        // The codex scenario: alice and bob are BOTH on the legacy bare marker
        // (shared account). Removing only alice must drop alice's env var but
        // NOT delete the shared keychain account bob still depends on.
        let entries = vec![entry("alice", "keychain:"), entry("bob", "keychain:")];
        let plan = plan_removal(&entries, NAME, |pid| pid == "alice");
        assert_eq!(plan.profiles_to_update, vec!["alice"]);
        assert!(
            plan.accounts_to_delete.is_empty(),
            "must NOT delete the shared bare account while bob still uses it"
        );
    }

    #[test]
    fn remove_plan_deletes_sole_bare_account() {
        // Only alice references the bare account → safe to delete it.
        let entries = vec![entry("alice", "keychain:")];
        let plan = plan_removal(&entries, NAME, |pid| pid == "alice");
        assert_eq!(plan.profiles_to_update, vec!["alice"]);
        assert_eq!(plan.accounts_to_delete, vec![NAME.to_string()]);
    }

    #[test]
    fn remove_plan_deletes_scoped_account_only_for_target() {
        // alice is scoped, bob is bare. Removing alice deletes alice's unique
        // scoped account and leaves bob's bare account intact.
        let entries = vec![
            entry("alice", "keychain:VERTEX_SA_JSON::alice"),
            entry("bob", "keychain:"),
        ];
        let plan = plan_removal(&entries, NAME, |pid| pid == "alice");
        assert_eq!(plan.profiles_to_update, vec!["alice"]);
        assert_eq!(
            plan.accounts_to_delete,
            vec!["VERTEX_SA_JSON::alice".to_string()]
        );
    }

    #[test]
    fn remove_plan_global_deletes_every_referenced_account() {
        let entries = vec![
            entry("alice", "keychain:VERTEX_SA_JSON::alice"),
            entry("bob", "keychain:"),
        ];
        let plan = plan_removal(&entries, NAME, |_| true);
        assert_eq!(plan.profiles_to_update.len(), 2);
        assert!(
            plan.accounts_to_delete
                .contains(&"VERTEX_SA_JSON::alice".to_string())
        );
        assert!(plan.accounts_to_delete.contains(&NAME.to_string()));
    }

    #[test]
    fn remove_plan_ignores_plaintext_values() {
        // A plaintext (non-marker) value isn't keychain-backed; remove-key
        // leaves it alone (no env removal, no keychain delete).
        let entries = vec![entry("alice", "sk-plaintext")];
        let plan = plan_removal(&entries, NAME, |_| true);
        assert!(plan.profiles_to_update.is_empty());
        assert!(plan.accounts_to_delete.is_empty());
    }
}
