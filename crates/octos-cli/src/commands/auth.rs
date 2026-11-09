//! Auth command: login, logout, status, and keychain management.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::Result;
use octos_agent::bridge::work_secret::{WorkSecret, WorkSecretGrantRecord, WorkSecretGrantStore};

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

    /// Store an API key in the OS secret store.
    ///
    /// macOS stores it in the Keychain; Linux in a 0600 file under
    /// `~/.octos/secrets`; Windows has no secret store yet — plain API keys
    /// can be passed via the environment or the profile's `env_vars`.
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
    /// Remove an API key from the OS secret store.
    ///
    /// macOS deletes the Keychain item; Linux the file under
    /// `~/.octos/secrets`. Only `"keychain:"`-marker entries are removed —
    /// plain `env_vars` values are left untouched; Windows has no secret
    /// store.
    #[command(name = "remove-key")]
    RemoveKey {
        /// Environment variable name to remove (e.g. OPENAI_API_KEY).
        name: String,
        /// Profile ID to update. If omitted, updates all profiles.
        #[arg(long, short)]
        profile: Option<String>,
    },

    /// Unlock the OS secret store for SSH sessions (macOS Keychain).
    ///
    /// Required before set-key/remove-key over SSH on macOS. With
    /// auto-login enabled, this is only needed once per boot. Linux's file
    /// store has no lock, so this is a no-op there; Windows has no secret
    /// store to unlock.
    #[command(name = "unlock")]
    Unlock {
        /// macOS login password (unused on Linux). If omitted, reads
        /// interactively.
        #[arg(long)]
        password: Option<String>,
    },

    /// Issue a short-lived session ingress secret for an external CLI agent.
    ///
    /// See docs/OCTOS_WORK_SECRET_SESSION_INGRESS.md.
    #[command(name = "issue-work-secret")]
    IssueWorkSecret {
        /// Session id the external agent may access.
        #[arg(long)]
        session: String,
        /// Grant lifetime, e.g. 15m, 1h, or 3600s.
        #[arg(long, default_value = "1h")]
        ttl: String,
        /// Public API base URL the guest should connect to.
        #[arg(long, default_value = "http://127.0.0.1:50080")]
        api_base_url: String,
        /// Profile id to bind on authenticated AppUI dispatch.
        #[arg(long)]
        profile: Option<String>,
        /// Data directory that `octos serve` uses.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Revoke a previously issued work secret.
    ///
    /// See docs/OCTOS_WORK_SECRET_SESSION_INGRESS.md.
    #[command(name = "revoke-work-secret")]
    RevokeWorkSecret {
        /// Encoded work secret or raw session_ingress_token.
        token_or_secret: String,
        /// Data directory that `octos serve` uses.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// List issued work secret grants (hashes only; the token is never stored).
    ///
    /// See docs/OCTOS_WORK_SECRET_SESSION_INGRESS.md.
    #[command(name = "list-work-secrets")]
    ListWorkSecrets {
        /// Data directory that `octos serve` uses.
        #[arg(long)]
        data_dir: Option<PathBuf>,
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
            AuthAction::IssueWorkSecret {
                session,
                ttl,
                api_base_url,
                profile,
                data_dir,
            } => issue_work_secret(&session, &ttl, &api_base_url, profile, data_dir),
            AuthAction::RevokeWorkSecret {
                token_or_secret,
                data_dir,
            } => revoke_work_secret(&token_or_secret, data_dir),
            AuthAction::ListWorkSecrets { data_dir } => list_work_secrets(data_dir),
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
    ProfileStore::open_unified(&home.join(".octos"))
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

/// #2234/45b — does this profile REFERENCE `name` as a credential?
/// Referenced = declared in env_vars, OR named by the LLM contract:
/// primary/fallback `route.api_key_env`, or any `sub_providers[].api_key_env`.
/// Route-declared keys are exactly the issue's zai-coding shape (env_vars
/// empty, primary route declaring ZAI_API_KEY).
fn profile_references_key(profile: &crate::profiles::UserProfile, name: &str) -> bool {
    if profile.config.env_vars.contains_key(name) {
        return true;
    }
    let llm = profile.config.llm.as_ref();
    let primary_route_env = llm
        .and_then(|l| l.primary.as_ref())
        .and_then(|sel| sel.route.as_ref())
        .and_then(|r| r.api_key_env.as_deref());
    if primary_route_env == Some(name) {
        return true;
    }
    if llm
        .map(|l| {
            l.fallbacks
                .iter()
                .any(|sel| sel.route.as_ref().and_then(|r| r.api_key_env.as_deref()) == Some(name))
        })
        .unwrap_or(false)
    {
        return true;
    }
    profile
        .config
        .sub_providers
        .iter()
        .any(|sp| sp.api_key_env.as_deref() == Some(name))
}

/// #2234/45c — read a SECRET without terminal echo.
///
/// Real tty → `rpassword` (safe termios ECHO-off wrapper; the workspace
/// `deny(unsafe_code)` rules out a hand-rolled termios arm). Non-tty
/// (piped tests / `echo | octos`) → plain read via the injected reader:
/// echo is moot off-terminal, and tests drive this arm deterministically.
fn read_secret_line<R: std::io::BufRead>(reader: R, prompt: &str) -> std::io::Result<String> {
    use std::io::Write as _;
    #[cfg(unix)]
    if unsafe_tty_check() {
        print!("{prompt}");
        std::io::stdout().flush()?;
        return rpassword::read_password();
    }
    let mut reader = reader;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// #2234/45c — tty probe without unsafe: `rpassword`'s public API has no
/// isatty, but /dev/tty reachability is a faithful proxy — when stdin is
/// redirected, opening the CONTROLLING tty succeeds while reading stdin
/// would come from the pipe (the case tests inject). Simple heuristic: a
/// Stdio `is_terminal()` (std, 1.70+, safe).
#[cfg(unix)]
fn unsafe_tty_check() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
}

/// Restore the account, not merely its existence. Never print credential bytes.
fn restore_secret(account: &str, previous: Option<&str>) -> Result<()> {
    match previous {
        Some(previous) => keychain::set_secret(account, previous),
        None => keychain::delete_secret(account).map(|_| ()),
    }
}

fn replace_secret(account: &str, secret: &str) -> Result<Option<String>> {
    let previous = keychain::get_secret(account)?;
    if let Err(store_error) = keychain::set_secret(account, secret) {
        // A backend can fail after changing the account (e.g. a sync failure).
        // Preserve the prior credential in that case too.
        return match restore_secret(account, previous.as_deref()) {
            Ok(()) => Err(eyre::eyre!(
                "secret store failed; rolled back the account: {store_error}"
            )),
            Err(rollback_error) => Err(eyre::eyre!(
                "secret store failed ({store_error}); rollback also failed ({rollback_error}); check the account before retrying"
            )),
        };
    }
    Ok(previous)
}

/// #2234/45c — injectable profile-save seam (issue Tests requested:
/// "Profile-save failure rolls back the newly stored secret"). Production
/// passes the direct store save; tests inject a failing closure to pin the
/// rollback without touching the real profile store.
fn set_key_with_save(
    name: &str,
    value: Option<String>,
    profile_id: Option<&str>,
    store: &ProfileStore,
    save_profile: impl Fn(&crate::profiles::UserProfile) -> Result<()>,
) -> Result<()> {
    // Get the secret value: from argument or interactive prompt
    let secret = match value {
        Some(v) => v,
        None => {
            let prompt = format!("Enter value for {}: ", name.cyan());
            let trimmed = read_secret_line(std::io::stdin().lock(), &prompt)?;
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

    // #2234/45b — the explicit-`--profile` guard runs BEFORE any secret is
    // written: an unreferenced name under an explicit profile id is almost
    // certainly a typo (the issue's exact failure mode), so refuse up front
    // instead of storing an orphan secret.
    let profiles = get_profiles(store, profile_id)?;
    if profile_id.is_some() && !profiles.iter().any(|p| profile_references_key(p, name)) {
        eyre::bail!(
            "profile '{}' does not reference '{}' (not in env_vars, no LLM route \
             api_key_env, no sub_provider api_key_env); refusing to store an \
             unreferenced secret — check the name or configure the profile first",
            profile_id.unwrap_or_default(),
            name
        );
    }

    // Shared keys: store once up front (also covers the orphan / no-profile
    // case). Scoped keys are stored per profile in the loop below.
    let shared_previous = if !scoped {
        replace_secret(name, &secret)?
    } else {
        None
    };

    // Update profile(s) to use the keychain marker
    let mut updated_count = 0;
    for mut profile in profiles {
        if profile_references_key(&profile, name) {
            let (account, marker) = keychain_target(name, &profile.id, &secret);
            let previous = if scoped {
                replace_secret(&account, &secret)?
            } else {
                shared_previous.clone()
            };
            profile.config.env_vars.insert(name.to_string(), marker);
            profile.updated_at = chrono::Utc::now();
            // Per-account rollback, not an all-profile transaction: earlier
            // successful saves remain committed. In particular, a shared key
            // must stay available once any successful save references it.
            if let Err(save_err) = save_profile(&profile) {
                if !scoped && updated_count > 0 {
                    return Err(eyre::eyre!(
                        "profile '{}' save failed; {updated_count} profile(s) updated; shared secret retained for those profiles: {save_err}",
                        profile.id
                    ));
                }
                if let Err(rollback_error) = restore_secret(&account, previous.as_deref()) {
                    return Err(eyre::eyre!(
                        "profile '{}' save failed ({save_err}); secret rollback also failed ({rollback_error}); {updated_count} profile(s) updated; check the account before retrying",
                        profile.id
                    ));
                }
                return Err(eyre::eyre!(
                    "profile '{}' save failed; rolled back that account's secret; {updated_count} profile(s) updated: {save_err}",
                    profile.id
                ));
            }
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

fn set_key(name: &str, value: Option<String>, profile_id: Option<&str>) -> Result<()> {
    let store = open_profile_store()?;
    set_key_with_save(name, value, profile_id, &store, |profile| {
        store.save(profile)
    })
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

    // #2234/45c — name the ACTIVE backend and its availability up front;
    // values are never printed (only marker-resolved account NAMES below).
    println!(
        "backend: {} ({})",
        keychain::backend_name(),
        if keychain::is_available() {
            "available"
        } else {
            "unavailable"
        }
    );

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
            eyre::bail!("{}", super::profile_not_found_message(&all_profiles, id));
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
        None => read_secret_line(std::io::stdin().lock(), "macOS login password: ")?,
    };

    keychain::unlock(&pw)?;

    println!(
        "{} Keychain unlocked (auto-lock disabled)",
        "OK".green().bold()
    );
    Ok(())
}

fn issue_work_secret(
    session: &str,
    ttl: &str,
    api_base_url: &str,
    profile: Option<String>,
    data_dir: Option<PathBuf>,
) -> Result<()> {
    let (secret, grant) = create_work_secret(session, ttl, api_base_url, profile, data_dir)?;
    // stdout stays the encoded secret and nothing else (scripts and the docs
    // both rely on that); operator-facing notes go to stderr.
    println!("{}", secret.encode()?);
    eprintln!(
        "Grant for session '{}' expires {} (ttl {}). Re-issuing for the same session replaces it: the earlier grant is removed, not kept as a revoked entry. Revoke early: octos auth revoke-work-secret '<secret>'. See docs/OCTOS_WORK_SECRET_SESSION_INGRESS.md.",
        display_id(session),
        grant.expires_at.format("%Y-%m-%d %H:%M:%S UTC"),
        format_ttl(grant.expires_at - grant.created_at),
    );
    Ok(())
}

fn create_work_secret(
    session: &str,
    ttl: &str,
    api_base_url: &str,
    profile: Option<String>,
    data_dir: Option<PathBuf>,
) -> Result<(WorkSecret, WorkSecretGrantRecord)> {
    let data_dir = super::resolve_data_dir(data_dir)?;
    let ttl = parse_ttl(ttl)?;
    let token = generate_ingress_token()?;
    let store = WorkSecretGrantStore::new(&data_dir);
    let grant = store.issue(session, &token, api_base_url, ttl, profile)?;
    Ok((WorkSecret::new(api_base_url, token), grant))
}

fn revoke_work_secret(token_or_secret: &str, data_dir: Option<PathBuf>) -> Result<()> {
    let data_dir = super::resolve_data_dir(data_dir)?;
    let token = WorkSecret::decode(token_or_secret)
        .map(|secret| secret.session_ingress_token)
        .unwrap_or_else(|_| token_or_secret.to_string());
    let store = WorkSecretGrantStore::new(&data_dir);
    if store.revoke_token(&token)? {
        println!("{} Revoked work secret", "OK".green().bold());
    } else {
        println!("No active work secret matched the provided token");
    }
    Ok(())
}

fn list_work_secrets(data_dir: Option<PathBuf>) -> Result<()> {
    let data_dir = super::resolve_data_dir(data_dir)?;
    let store = WorkSecretGrantStore::new(&data_dir);
    let grants = store.list()?;
    if grants.is_empty() {
        println!("No work secret grants in {}.", store.path().display());
        return Ok(());
    }
    let now = chrono::Utc::now();
    println!(
        "{}",
        format!("Work secret grants in {}:", store.path().display()).bold()
    );
    for grant in &grants {
        println!("{}", format_work_secret_grant(grant, now));
    }
    Ok(())
}

/// Session and profile ids are user-controlled (they end up in the grant
/// file), so control characters are rendered inert before they reach terminal
/// or log output — one grant line must not be able to forge another.
fn display_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// One line of `list-work-secrets` output, computed purely (no I/O) so it's
/// unit-testable. Only the token hash prefix is shown — the grant store never
/// holds the token itself.
fn format_work_secret_grant(
    grant: &WorkSecretGrantRecord,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let status = match grant.revoked_at {
        Some(at) => format!("revoked {}", at.format("%Y-%m-%d %H:%M:%S UTC"))
            .red()
            .to_string(),
        None if !grant.active(now) => "expired".yellow().to_string(),
        None => "active".green().to_string(),
    };
    let hash_prefix = grant.token_hash.get(..12).unwrap_or(&grant.token_hash);
    format!(
        "  {}: {} [profile {}] hash {}… created {} expires {} ttl {}",
        display_id(&grant.session_id).cyan(),
        status,
        grant
            .profile_id
            .as_deref()
            .map(display_id)
            .unwrap_or_else(|| "-".to_string()),
        hash_prefix,
        grant.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
        grant.expires_at.format("%Y-%m-%d %H:%M:%S UTC"),
        format_ttl(grant.expires_at - grant.created_at),
    )
}

/// Render a ttl the way `parse_ttl` accepts it (largest exact suffix wins).
/// Durations outside `parse_ttl`'s domain (zero or negative, only reachable
/// via a hand-edited grant file) render verbatim with an `s` suffix.
fn format_ttl(ttl: chrono::Duration) -> String {
    let seconds = ttl.num_seconds();
    if seconds <= 0 {
        return format!("{seconds}s");
    }
    if seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn generate_ingress_token() -> Result<String> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| eyre::eyre!(error))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_ttl(input: &str) -> Result<chrono::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        eyre::bail!("ttl cannot be empty");
    }
    let (number, multiplier) = match trimmed.chars().last().unwrap() {
        's' | 'S' => (&trimmed[..trimmed.len() - 1], 1),
        'm' | 'M' => (&trimmed[..trimmed.len() - 1], 60),
        'h' | 'H' => (&trimmed[..trimmed.len() - 1], 60 * 60),
        'd' | 'D' => (&trimmed[..trimmed.len() - 1], 24 * 60 * 60),
        _ => (trimmed, 1),
    };
    let value: i64 = number
        .parse()
        .map_err(|_| eyre::eyre!("ttl must be a positive integer with optional s/m/h/d suffix"))?;
    if value <= 0 {
        eyre::bail!("ttl must be positive");
    }
    Ok(chrono::Duration::seconds(value.saturating_mul(multiplier)))
}

/// Get profiles matching the optional filter, or all profiles.
fn get_profiles(
    store: &ProfileStore,
    profile_id: Option<&str>,
) -> Result<Vec<crate::profiles::UserProfile>> {
    if let Some(id) = profile_id {
        match store.get(id)? {
            Some(p) => Ok(vec![p]),
            // If listing the registry fails too, surface that error rather
            // than a hint built from an unknown set of profiles.
            None => {
                let existing = store.list()?;
                eyre::bail!("{}", super::profile_not_found_message(&existing, id))
            }
        }
    } else {
        store.list()
    }
}
