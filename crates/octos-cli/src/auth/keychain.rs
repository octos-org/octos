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

/// Sentinel value stored in profile `env_vars` to indicate
/// that the real secret lives in the macOS Keychain.
pub const KEYCHAIN_MARKER: &str = "keychain:";

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
                Ok(Some(secret))
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
    if value != KEYCHAIN_MARKER {
        return Some(value.to_string());
    }
    match get_secret(name) {
        Ok(Some(secret)) => Some(secret),
        Ok(None) => {
            tracing::warn!(
                var = %name,
                "keychain marker found but no secret stored in keychain"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                var = %name,
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
