//! macOS Keychain integration for secure API key storage.
//!
//! Uses the macOS `security` CLI to store secrets in the login keychain.
//! This bypasses application-level ACL prompts that would block on headless
//! servers (the `keyring` crate's native API requires GUI confirmation for
//! new applications).
//!
//! ## SSH access
//!
//! SSH sessions cannot access a locked keychain.  Call [`unlock`] with the
//! login password first, or enable auto-login so the keychain is unlocked
//! at boot.

use std::collections::HashMap;

use eyre::{Result, WrapErr};

/// Sentinel prefix stored in profile `env_vars` to indicate that the real
/// secret lives in the macOS Keychain.
///
/// Two forms exist:
/// - **bare** `"keychain:"` — legacy / single-account: the keychain account is
///   the env-var name itself. Still read for backward compatibility.
/// - **scoped** `"keychain:<account>"` — the suffix is the exact keychain
///   account to read, which lets per-profile secrets (e.g. each tenant's Vertex
///   service account) live under distinct accounts so saves can't collide.
pub const KEYCHAIN_MARKER: &str = "keychain:";

/// Env-var credentials that must live in the OS keychain, never in plaintext
/// profile config (each carries a private key — e.g. the Vertex service-account
/// JSON). Stored under per-profile [`scoped_account`]s so tenants can't collide.
///
/// Lives here (not in the `api`-gated module) so non-API CLI commands
/// (`octos auth set-key/keys/remove-key`) can scope the same set consistently.
pub const KEYCHAIN_BACKED_ENV_VARS: &[&str] = &["VERTEX_SA_JSON"];

/// Whether `value` is a raw Google service-account JSON (i.e. carries a private
/// key). Detected by **content**, not by the env-var name, so the credential is
/// protected even when stored under a non-whitelisted name (e.g. a dashboard
/// "Custom" provider that derives `VERTEX_API_KEY`).
pub fn is_service_account_json(value: &str) -> bool {
    let v = value.trim_start();
    v.starts_with('{') && value.contains("\"private_key\"") && value.contains("service_account")
}

/// Whether the stored env var (`key` = `value`) holds a raw secret that must be
/// relocated into the keychain before persisting: a declared keychain-backed var
/// holding a raw value, OR any var whose value is a service-account JSON. The
/// content check closes the bypass of saving the SA key under a custom name.
pub fn needs_keychain_relocation(key: &str, value: &str) -> bool {
    is_service_account_json(value)
        || (KEYCHAIN_BACKED_ENV_VARS.contains(&key) && value.trim_start().starts_with('{'))
}

/// Whether `value` is any keychain marker (bare or scoped).
pub fn is_marker(value: &str) -> bool {
    value.starts_with(KEYCHAIN_MARKER)
}

/// The keychain account name for `env_var` scoped to `profile_id`. Distinct
/// profiles get distinct accounts, so two profiles saving different secrets
/// under the same env var no longer overwrite a single shared keychain item.
pub fn scoped_account(env_var: &str, profile_id: &str) -> String {
    format!("{env_var}::{profile_id}")
}

/// Build the marker stored in profile config for a given keychain `account`.
pub fn marker_for(account: &str) -> String {
    format!("{KEYCHAIN_MARKER}{account}")
}

/// The keychain account a marker `value` points at: the scoped suffix when
/// present, else `fallback_name` (the env-var name) for a legacy bare marker.
pub fn marker_account<'a>(value: &'a str, fallback_name: &'a str) -> &'a str {
    value
        .strip_prefix(KEYCHAIN_MARKER)
        .filter(|account| !account.is_empty())
        .unwrap_or(fallback_name)
}

/// The service name used for all octos keychain entries.
const SERVICE: &str = "octos";

/// Unlock the login keychain so subsequent operations succeed from SSH.
///
/// Also disables auto-lock so the keychain stays unlocked until reboot.
pub fn unlock(password: &str) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let keychain_path = format!("{home}/Library/Keychains/login.keychain-db");

    let out = std::process::Command::new("security")
        .args(["unlock-keychain", "-p", password, &keychain_path])
        .output()
        .wrap_err("failed to run security unlock-keychain")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        eyre::bail!("failed to unlock keychain: {err}");
    }

    // Disable auto-lock so it stays unlocked until reboot
    let _ = std::process::Command::new("security")
        .args(["set-keychain-settings", &keychain_path])
        .output();

    Ok(())
}

/// Store a secret in the macOS Keychain.
///
/// Uses `security add-generic-password` which works without GUI prompts.
/// Handles updates by deleting existing entries first.
pub fn set_secret(name: &str, secret: &str) -> Result<()> {
    // Delete all existing entries for this name
    loop {
        let out = std::process::Command::new("security")
            .args(["delete-generic-password", "-s", SERVICE, "-a", name])
            .output();
        match out {
            Ok(o) if o.status.success() => continue,
            _ => break,
        }
    }

    // Add new entry
    let out = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            SERVICE,
            "-a",
            name,
            "-w",
            secret,
        ])
        .output()
        .wrap_err("failed to run security add-generic-password")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        eyre::bail!("failed to store {name} in keychain: {err}");
    }
    Ok(())
}

/// `security find-generic-password -w` prints the password as a **hex string**
/// (no marker) whenever it contains non-printable bytes — notably the newlines
/// in a service-account JSON. Decode that back to text.
///
/// Decoding is deliberately scoped to the multi-line case: we only decode when
/// the whole string is even-length ASCII hex, the bytes are valid UTF-8, AND
/// the decoded text contains a newline. `security` hex-encodes a stored secret
/// *only* when it can't emit it verbatim on one line (i.e. it has newlines), so
/// a single-line secret that happens to be valid even-length ASCII hex (e.g.
/// `41424344`) was returned as-is and must NOT be decoded — doing so would
/// silently corrupt it into `ABCD`. Requiring a newline removes that ambiguity.
fn decode_security_hex(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() < 2 || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    let decoded = String::from_utf8(bytes?).ok()?;
    // Only a genuinely multi-line secret would have been hex-encoded by
    // `security`; refuse single-line values to avoid corrupting hex-shaped keys.
    decoded.contains('\n').then_some(decoded)
}

/// Retrieve a secret from the macOS Keychain.
///
/// Returns `Ok(Some(secret))` on success, `Ok(None)` if not found,
/// or `Err` on unexpected failures (keychain locked, etc.).
///
/// Uses a 3-second timeout to prevent hanging on headless servers.
pub fn get_secret(name: &str) -> Result<Option<String>> {
    let name_owned = name.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                SERVICE,
                "-a",
                &name_owned,
                "-w",
            ])
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(out)) if out.status.success() => {
            let secret = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if secret.is_empty() {
                Ok(None)
            } else {
                // `security` may have hex-encoded a multi-line secret (e.g. SA JSON).
                Ok(Some(decode_security_hex(&secret).unwrap_or(secret)))
            }
        }
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("could not be found") || err.contains("SecKeychainSearchCopyNext") {
                Ok(None)
            } else {
                Err(eyre::eyre!("keychain lookup failed for {name}: {err}"))
            }
        }
        Ok(Err(e)) => Err(eyre::eyre!("failed to run security command: {e}")),
        Err(_) => Err(eyre::eyre!(
            "keychain lookup timed out for {name} (keychain may be locked)"
        )),
    }
}

/// Delete a secret from the macOS Keychain.
///
/// Returns `Ok(true)` if deleted, `Ok(false)` if not found.
pub fn delete_secret(name: &str) -> Result<bool> {
    let mut deleted = false;
    loop {
        let out = std::process::Command::new("security")
            .args(["delete-generic-password", "-s", SERVICE, "-a", name])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                deleted = true;
                continue;
            }
            _ => break,
        }
    }
    Ok(deleted)
}

/// Check if the keychain is accessible (unlocked).
pub fn is_accessible() -> bool {
    // Try to add and immediately delete a test entry
    let out = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            "octos-access-test",
            "-a",
            "test",
            "-w",
            "test",
        ])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            // Clean up
            let _ = std::process::Command::new("security")
                .args([
                    "delete-generic-password",
                    "-s",
                    "octos-access-test",
                    "-a",
                    "test",
                ])
                .output();
            true
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            // "already exists" also means accessible
            err.contains("already exists")
        }
        Err(_) => false,
    }
}

/// Resolve a single env var value: if it equals [`KEYCHAIN_MARKER`],
/// look up the real secret from the Keychain.  Otherwise return the
/// value as-is.
///
/// On keychain failure, logs a warning and returns `None`.
pub fn resolve_value(name: &str, value: &str) -> Option<String> {
    if !is_marker(value) {
        return Some(value.to_string());
    }
    // Scoped markers carry the account in the suffix; bare ones fall back to the
    // env-var name. This keeps existing single-account entries working.
    let account = marker_account(value, name);
    match get_secret(account) {
        Ok(Some(secret)) => Some(secret),
        Ok(None) => {
            tracing::warn!(
                var = %name,
                account = %account,
                "keychain marker found but no secret stored in keychain"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                var = %name,
                account = %account,
                error = %e,
                "failed to read secret from keychain, skipping"
            );
            None
        }
    }
}

/// Resolve all `"keychain:"` markers in an env_vars map.
///
/// Returns a new `HashMap` with real secrets substituted in.
/// Entries that fail to resolve are omitted (logged as warnings).
pub fn resolve_env_vars(env_vars: &HashMap<String, String>) -> HashMap<String, String> {
    let mut resolved = HashMap::with_capacity(env_vars.len());
    for (key, value) in env_vars {
        if let Some(real_value) = resolve_value(key, value) {
            resolved.insert(key.clone(), real_value);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_hex_password_emitted_by_security_for_multiline_secret() {
        // `security -w` hex-encodes a value containing newlines (e.g. SA JSON).
        let json = "{\n  \"type\": \"service_account\",\n  \"project_id\": \"p\"\n}";
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode_security_hex(&hex).as_deref(), Some(json));
    }

    #[test]
    fn leaves_ordinary_api_key_untouched() {
        // Contains non-hex characters → not decoded.
        assert!(decode_security_hex("sk-proj-abc123XYZ").is_none());
    }

    #[test]
    fn leaves_odd_length_or_non_hex_untouched() {
        assert!(decode_security_hex("abc").is_none()); // odd length
        assert!(decode_security_hex("zzzz").is_none()); // non-hex chars
    }

    #[test]
    fn leaves_binary_hex_untouched_when_not_utf8() {
        // Pure hex that decodes to non-UTF-8 bytes is left as-is.
        assert!(decode_security_hex("deadbeef").is_none());
    }

    #[test]
    fn leaves_single_line_ascii_hex_secret_untouched() {
        // A real secret that is even-length ASCII hex and valid UTF-8 but
        // single-line (e.g. "41424344") was returned verbatim by `security`,
        // never hex-encoded — decoding it would corrupt it into "ABCD".
        assert_eq!(decode_security_hex("41424344"), None);
    }

    #[test]
    fn test_resolve_value_passthrough() {
        // Non-marker values pass through unchanged
        assert_eq!(resolve_value("FOO", "bar"), Some("bar".to_string()));
        assert_eq!(
            resolve_value("KEY", "sk-proj-abc123"),
            Some("sk-proj-abc123".to_string())
        );
        assert_eq!(resolve_value("EMPTY", ""), Some(String::new()));
    }

    #[test]
    fn test_resolve_env_vars_passthrough() {
        let mut env = HashMap::new();
        env.insert("A".into(), "val_a".into());
        env.insert("B".into(), "val_b".into());

        let resolved = resolve_env_vars(&env);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved["A"], "val_a");
        assert_eq!(resolved["B"], "val_b");
    }

    #[test]
    fn test_keychain_marker_constant() {
        assert_eq!(KEYCHAIN_MARKER, "keychain:");
    }

    #[test]
    fn scoped_account_and_marker_roundtrip() {
        let acct = scoped_account("VERTEX_SA_JSON", "alice");
        assert_eq!(acct, "VERTEX_SA_JSON::alice");
        assert_eq!(marker_for(&acct), "keychain:VERTEX_SA_JSON::alice");
        assert!(is_marker(&marker_for(&acct)));
        assert!(is_marker("keychain:")); // legacy bare marker
        assert!(!is_marker("sk-proj-abc123"));
    }

    #[test]
    fn detects_service_account_json_by_content_regardless_of_name() {
        let sa = r#"{"type":"service_account","private_key":"x","project_id":"p"}"#;
        assert!(is_service_account_json(sa));
        // Leading whitespace is tolerated.
        assert!(is_service_account_json(&format!("  \n{sa}")));
        // Ordinary API keys / non-SA JSON are not flagged.
        assert!(!is_service_account_json("sk-proj-abc123"));
        assert!(!is_service_account_json(r#"{"model":"x","temperature":1}"#));
        assert!(!is_service_account_json(""));
    }

    #[test]
    fn needs_relocation_closes_custom_env_name_bypass() {
        let sa = r#"{"type":"service_account","private_key":"x"}"#;
        // The declared name with a raw value → relocate.
        assert!(needs_keychain_relocation("VERTEX_SA_JSON", sa));
        // SA JSON under a CUSTOM name (the dashboard bypass) → still relocate.
        assert!(needs_keychain_relocation("VERTEX_API_KEY", sa));
        assert!(needs_keychain_relocation("ANYTHING", sa));
        // A plain key, or a non-SA JSON under a non-whitelisted name → leave it.
        assert!(!needs_keychain_relocation("VERTEX_API_KEY", "sk-plain"));
        assert!(!needs_keychain_relocation("OPENAI_API_KEY", r#"{"a":1}"#));
        // An already-relocated marker is not a raw value → no relocation.
        assert!(!needs_keychain_relocation(
            "VERTEX_SA_JSON",
            "keychain:VERTEX_SA_JSON::alice"
        ));
    }

    #[test]
    fn marker_account_prefers_scope_else_falls_back_to_name() {
        // Scoped marker → account taken from the suffix (per-profile isolation).
        assert_eq!(
            marker_account("keychain:VERTEX_SA_JSON::alice", "VERTEX_SA_JSON"),
            "VERTEX_SA_JSON::alice"
        );
        // Bare legacy marker → the env-var name (backward compatible).
        assert_eq!(
            marker_account("keychain:", "VERTEX_SA_JSON"),
            "VERTEX_SA_JSON"
        );
    }

    #[test]
    #[ignore = "requires macOS Keychain access"]
    fn keychain_integration_scoped_accounts_dont_collide() {
        // Real-keychain proof of the P1 fix: two profiles' scoped accounts are
        // independent — writing/deleting one never affects the other.
        let alice = scoped_account("VERTEX_SA_JSON", "alice-itest");
        let bob = scoped_account("VERTEX_SA_JSON", "bob-itest");
        let _ = delete_secret(&alice);
        let _ = delete_secret(&bob);

        set_secret(&alice, "alice-key").unwrap();
        set_secret(&bob, "bob-key").unwrap();
        assert_eq!(get_secret(&alice).unwrap().as_deref(), Some("alice-key"));
        assert_eq!(
            get_secret(&bob).unwrap().as_deref(),
            Some("bob-key"),
            "bob's account must not be overwritten by alice's"
        );

        // Deleting alice's account leaves bob's intact.
        delete_secret(&alice).unwrap();
        assert!(get_secret(&alice).unwrap().is_none());
        assert_eq!(get_secret(&bob).unwrap().as_deref(), Some("bob-key"));

        delete_secret(&bob).unwrap();
    }

    // Integration tests that require a real Keychain session.
    // Run manually with: cargo test -p octos-cli keychain_integration -- --ignored
    #[test]
    #[ignore = "requires macOS Keychain access"]
    fn keychain_integration_roundtrip() {
        let name = "octos-test-key";
        let secret = "test-secret-value-12345";

        // Clean up from any previous failed run
        let _ = delete_secret(name);

        // Set
        set_secret(name, secret).expect("set_secret should succeed");

        // Get
        let retrieved = get_secret(name)
            .expect("get_secret should succeed")
            .expect("secret should exist");
        assert_eq!(retrieved, secret);

        // Resolve via marker
        let resolved = resolve_value(name, KEYCHAIN_MARKER);
        assert_eq!(resolved, Some(secret.to_string()));

        // Delete
        let deleted = delete_secret(name).expect("delete should succeed");
        assert!(deleted, "should report deletion");

        // Verify gone
        let after = get_secret(name).expect("get after delete should succeed");
        assert!(after.is_none(), "should be None after deletion");

        // Delete again (no-op)
        let deleted_again = delete_secret(name).expect("re-delete should succeed");
        assert!(!deleted_again, "should report not found");
    }

    #[test]
    #[ignore = "requires macOS Keychain access"]
    fn keychain_integration_resolve_env_vars() {
        let name = "octos-test-resolve";
        let secret = "resolved-secret";
        let _ = delete_secret(name);

        set_secret(name, secret).unwrap();

        let mut env = HashMap::new();
        env.insert(name.into(), KEYCHAIN_MARKER.into());
        env.insert("PLAIN".into(), "literal".into());

        let resolved = resolve_env_vars(&env);
        assert_eq!(resolved[name], secret);
        assert_eq!(resolved["PLAIN"], "literal");

        delete_secret(name).unwrap();
    }
}
