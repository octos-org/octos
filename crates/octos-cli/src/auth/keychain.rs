//! Platform-neutral secret store for API keys (#2234).
//!
//! Backends, selected at compile time:
//! * **macOS** — the login keychain via the `security` CLI. This bypasses
//!   application-level ACL prompts that would block on headless servers (the
//!   `keyring` crate's native API requires GUI confirmation for new
//!   applications). SSH sessions may need [`unlock`] with the login password
//!   first (see the macOS notes below).
//! * **Linux** — a file-backed store under `<octos home>/secrets` (directory
//!   0700, one file per key 0600): the lowest-dependency option that works in
//!   headless sessions with no D-Bus / Secret Service. [`unlock`] is a no-op.
//! * **Other platforms** — [`set_secret`] / [`get_secret`] / [`delete_secret`]
//!   return an explicit "secret store unsupported" error. There is never a
//!   silent fallback to plaintext profile config.

use std::collections::HashMap;

use eyre::Result;
// #2258 — `wrap_err` is only used by the macOS-gated helpers below; keep the
// trait import gated too so Linux clippy (unused_imports) and macOS rustc agree.
#[cfg(target_os = "macos")]
use eyre::WrapErr;

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

/// The service name used for all octos keychain entries (macOS backend).
#[cfg(target_os = "macos")]
const SERVICE: &str = "octos";

/// Human-readable name of the active secret-store backend.
pub fn backend_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos-keychain"
    }
    #[cfg(target_os = "linux")]
    {
        "linux-file"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unsupported"
    }
}

/// Whether a secret-store backend exists on this platform.
pub fn is_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        linux_file::is_available()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Explicit error for platforms with no secret-store backend (#2234).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported_store_error(op: &str) -> eyre::Report {
    // TODO(#2234): implement a native Windows secret store (keyring/windows-native).
    eyre::eyre!(
        "secret store unsupported on {} ({op} failed)",
        std::env::consts::OS
    )
}

/// Unlock the login keychain so subsequent operations succeed from SSH.
///
/// No-op on Linux (the file store has no lock); unsupported elsewhere.
pub fn unlock(password: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos_unlock(password)
    }
    #[cfg(target_os = "linux")]
    {
        // The file-backed store is always usable when the home dir resolves;
        // there is no keychain lock to open.
        let _ = password;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = password;
        Err(unsupported_store_error("unlock"))
    }
}

/// Store a secret in the platform secret store.
pub fn set_secret(name: &str, secret: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos_set_secret(name, secret)
    }
    #[cfg(target_os = "linux")]
    {
        linux_file::set_secret(name, secret)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (name, secret);
        Err(unsupported_store_error("set_secret"))
    }
}

/// Retrieve a secret from the platform secret store.
///
/// Returns `Ok(Some(secret))` on success, `Ok(None)` if not found,
/// or `Err` on unexpected failures (keychain locked, etc.).
pub fn get_secret(name: &str) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        macos_get_secret(name)
    }
    #[cfg(target_os = "linux")]
    {
        linux_file::get_secret(name)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = name;
        Err(unsupported_store_error("get_secret"))
    }
}

/// Delete a secret from the platform secret store.
///
/// Returns `Ok(true)` if deleted, `Ok(false)` if not found.
pub fn delete_secret(name: &str) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos_delete_secret(name)
    }
    #[cfg(target_os = "linux")]
    {
        linux_file::delete_secret(name)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = name;
        Err(unsupported_store_error("delete_secret"))
    }
}

/// Check if the secret store is accessible (unlocked / usable).
pub fn is_accessible() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_is_accessible()
    }
    #[cfg(target_os = "linux")]
    {
        linux_file::is_accessible()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

// ── Linux file-backed store ────────────────────────────────────────────────
//
// `<octos home>/secrets/<account>` — one file per secret, 0600, directory
// 0700. The root mirrors the `ProfileStore::octos_home_dir()` the CLI auth
// commands use (`~/.octos`): same resolver, `secrets/` sibling of `profiles/`.
#[cfg(target_os = "linux")]
pub(crate) mod linux_file {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use eyre::{Result, WrapErr};

    const DIR_MODE: u32 = 0o700;
    const FILE_MODE: u32 = 0o600;

    #[cfg(test)]
    thread_local! {
        // #2234: thread-local override — a global single-slot TEST_ROOT let
        // PARALLEL tests stomp each other's temp root (one drops, the next
        // op in a sibling test hits ENOENT). Per-thread slots make every
        // test's injection independent; cargo's thread-per-test model
        // guarantees isolation.
        static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Test-only pin of the secrets root. The guard serializes concurrent
    /// tests (they contend on the same mutex) and restores the default
    /// resolution when dropped.
    #[cfg(test)]
    /// #2234: RootGuard holds NOTHING (lock-free) — the override is set on
    /// entry and cleared on drop via short lock acquisitions only, so
    /// secrets_root's read never contends with a held lock (same-thread
    /// re-entrant deadlock was the roundtrip hang).
    pub(crate) fn override_root_for_tests(dir: PathBuf) -> RootGuard {
        TEST_ROOT.with(|slot| *slot.borrow_mut() = Some(dir));
        RootGuard
    }

    #[cfg(test)]
    pub(crate) struct RootGuard;

    #[cfg(test)]
    impl Drop for RootGuard {
        fn drop(&mut self) {
            TEST_ROOT.with(|slot| *slot.borrow_mut() = None);
        }
    }

    /// The secrets root: `<octos home>/secrets`, with the octos home resolved
    /// exactly as `ProfileStore::octos_home_dir()` does for the CLI auth
    /// commands (`~/.octos`).
    fn secrets_root() -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(dir) = TEST_ROOT.with(|slot| slot.borrow().clone()) {
            return Ok(dir);
        }
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("cannot determine home directory for the secret store"))?;
        Ok(home.join(".octos").join("secrets"))
    }

    /// Account names are env-var names or `<ENV>::<profile_id>`; reject
    /// anything that could escape the secrets directory.
    fn validate_name(name: &str) -> Result<()> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
            eyre::bail!("invalid secret account name: {name:?}");
        }
        Ok(())
    }

    fn ensure_root(root: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(root)
            .wrap_err_with(|| format!("failed to create secrets dir: {}", root.display()))?;
        // Re-assert 0700 even when the dir already existed (umask could have
        // loosened it at creation).
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(DIR_MODE))
            .wrap_err_with(|| format!("failed to restrict secrets dir: {}", root.display()))?;
        Ok(())
    }

    pub fn is_available() -> bool {
        secrets_root().is_ok()
    }

    pub fn is_accessible() -> bool {
        secrets_root().and_then(|root| ensure_root(&root)).is_ok()
    }

    pub fn set_secret(name: &str, secret: &str) -> Result<()> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        validate_name(name)?;
        let root = secrets_root()?;
        ensure_root(&root)?;
        let path = root.join(name);
        // mode() applies 0600 atomically at creation; the later
        // set_permissions re-asserts it in case a umask quirk loosened it.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&path)
            .wrap_err_with(|| format!("failed to create secret file: {}", path.display()))?;
        file.write_all(secret.as_bytes())
            .wrap_err_with(|| format!("failed to write secret file: {}", path.display()))?;
        file.sync_all()
            .wrap_err_with(|| format!("failed to sync secret file: {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(FILE_MODE))
            .wrap_err_with(|| format!("failed to restrict secret file: {}", path.display()))?;
        Ok(())
    }

    pub fn get_secret(name: &str) -> Result<Option<String>> {
        validate_name(name)?;
        let path = secrets_root()?.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(eyre::eyre!(
                    "failed to read secret file {}: {e}",
                    path.display()
                ));
            }
        };
        String::from_utf8(bytes)
            .map(Some)
            .wrap_err_with(|| format!("secret file {} is not valid UTF-8", path.display()))
    }

    pub fn delete_secret(name: &str) -> Result<bool> {
        validate_name(name)?;
        let path = secrets_root()?.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(eyre::eyre!(
                "failed to delete secret file {}: {e}",
                path.display()
            )),
        }
    }
}

/// Test hook: pin the Linux secrets root to a temp dir (no-op elsewhere).
#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux_file::override_root_for_tests as test_override_secrets_root;

/// Unlock the login keychain so subsequent operations succeed from SSH.
///
/// Also disables auto-lock so the keychain stays unlocked until reboot.
#[cfg(target_os = "macos")]
fn macos_unlock(password: &str) -> Result<()> {
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
#[cfg(target_os = "macos")]
fn macos_set_secret(name: &str, secret: &str) -> Result<()> {
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
fn macos_get_secret(name: &str) -> Result<Option<String>> {
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
#[cfg(target_os = "macos")]
fn macos_delete_secret(name: &str) -> Result<bool> {
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
#[cfg(target_os = "macos")]
fn macos_is_accessible() -> bool {
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
    fn backend_name_matches_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(backend_name(), "macos-keychain");
        #[cfg(target_os = "linux")]
        assert_eq!(backend_name(), "linux-file");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert_eq!(backend_name(), "unsupported");
    }

    #[test]
    fn availability_matches_backend_presence() {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(is_available());
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(!is_available());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_file_backend_roundtrip_with_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = test_override_secrets_root(tmp.path().to_path_buf());

        // Full CRUD against the injected root — the `security` binary does
        // not exist on Linux, so success proves no macOS CLI path runs.
        set_secret("ZAI_API_KEY", "sk-test-123").unwrap();
        assert_eq!(
            get_secret("ZAI_API_KEY").unwrap().as_deref(),
            Some("sk-test-123")
        );

        // Directory 0700, per-key file 0600.
        let dir_mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = std::fs::metadata(tmp.path().join("ZAI_API_KEY"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);

        // Overwrite updates in place (still 0600); scoped accounts map to
        // their own file; multiline SA JSON round-trips byte-exact.
        set_secret("ZAI_API_KEY", "sk-second").unwrap();
        assert_eq!(
            get_secret("ZAI_API_KEY").unwrap().as_deref(),
            Some("sk-second")
        );
        let sa = "{\n  \"type\": \"service_account\",\n  \"private_key\": \"x\"\n}";
        let acct = scoped_account("VERTEX_SA_JSON", "alice");
        set_secret(&acct, sa).unwrap();
        assert_eq!(get_secret(&acct).unwrap().as_deref(), Some(sa));

        // Delete: true once, then false (not found); read back None.
        assert!(delete_secret("ZAI_API_KEY").unwrap());
        assert!(!delete_secret("ZAI_API_KEY").unwrap());
        assert_eq!(get_secret("ZAI_API_KEY").unwrap(), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_file_backend_rejects_path_escaping_names() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = test_override_secrets_root(tmp.path().to_path_buf());
        assert!(set_secret("../evil", "x").is_err());
        assert!(set_secret("a/b", "x").is_err());
        assert!(set_secret("", "x").is_err());
        // Nothing was written outside the root.
        assert!(
            std::fs::read_dir(tmp.path().parent().unwrap())
                .unwrap()
                .all(|e| e.unwrap().path() != tmp.path().with_file_name("evil"))
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_unlock_is_noop() {
        assert!(unlock("any-password").is_ok());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn decodes_hex_password_emitted_by_security_for_multiline_secret() {
        // `security -w` hex-encodes a value containing newlines (e.g. SA JSON).
        let json = "{\n  \"type\": \"service_account\",\n  \"project_id\": \"p\"\n}";
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode_security_hex(&hex).as_deref(), Some(json));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn leaves_ordinary_api_key_untouched() {
        // Contains non-hex characters → not decoded.
        assert!(decode_security_hex("sk-proj-abc123XYZ").is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn leaves_odd_length_or_non_hex_untouched() {
        assert!(decode_security_hex("abc").is_none()); // odd length
        assert!(decode_security_hex("zzzz").is_none()); // non-hex chars
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn leaves_binary_hex_untouched_when_not_utf8() {
        // Pure hex that decodes to non-UTF-8 bytes is left as-is.
        assert!(decode_security_hex("deadbeef").is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
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
