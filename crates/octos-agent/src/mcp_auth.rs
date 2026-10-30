//! OAuth 2.1 support for MCP streamable-HTTP servers.
//!
//! Two halves:
//! - **Storage**: persist/load access+refresh tokens in the OS keyring, keyed by
//!   a hash of the server URL. Reused by both the runtime connect path and the
//!   interactive `octos mcp login` command.
//! - **Runtime connect** ([`connect_oauth`]): load stored tokens, build an rmcp
//!   `AuthClient` streamable-HTTP transport, and hand back a live session. rmcp
//!   refreshes the access token automatically when it nears expiry.
//! - **Login** ([`login`]): the interactive authorization-code flow — metadata
//!   discovery + dynamic client registration + PKCE (all inside rmcp's
//!   `OAuthState`), a browser consent open, and a `tiny_http` loopback catcher
//!   for the redirect. The resulting tokens are written to the keyring.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use eyre::Result;
use rmcp::model::ClientInfo;
use rmcp::service::serve_client;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthState, OAuthTokenResponse};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use crate::mcp::{McpServerConfig, McpService};

/// OS keyring service name under which octos stores MCP OAuth tokens.
pub const KEYRING_SERVICE: &str = "octos MCP Credentials";

/// How long to wait for the OAuth `initialize` handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the user to complete the browser consent.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Tokens persisted for one MCP server (JSON-serialized into the keyring entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    /// The server URL these tokens authorize.
    pub url: String,
    /// The (dynamically-registered or configured) OAuth client id.
    pub client_id: String,
    /// The full oauth2 token response, serialized (access + refresh + type).
    pub token_response: serde_json::Value,
    /// Wall-clock expiry (ms since epoch), derived from `expires_in` at save
    /// time so a reload can tell whether the token still looks valid.
    pub expires_at_ms: Option<u64>,
}

/// Stable keyring key for a server URL: `"<normalized-url>|<sha256[..16]>"`.
///
/// Normalizes only the case-insensitive parts (scheme + host) via `url::Url`
/// while preserving path/query case — `https://h/MCP` and `https://h/mcp` are
/// *different* endpoints and must not share (or clobber) a token entry.
pub fn keyring_key(url: &str) -> String {
    let normalized = url::Url::parse(url)
        .map(|u| u.as_str().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| url.trim_end_matches('/').to_string());
    let digest = Sha256::digest(normalized.as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{normalized}|{short}")
}

/// Persist tokens for a server into the OS keyring.
pub fn save_tokens(url: &str, tokens: &StoredTokens) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_key(url))
        .map_err(|e| eyre::eyre!("open keyring entry: {e}"))?;
    let json = serde_json::to_string(tokens).map_err(|e| eyre::eyre!("serialize tokens: {e}"))?;
    entry
        .set_password(&json)
        .map_err(|e| eyre::eyre!("write tokens to keyring: {e}"))?;
    Ok(())
}

/// Load tokens for a server from the OS keyring, if present.
pub fn load_tokens(url: &str) -> Result<Option<StoredTokens>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_key(url))
        .map_err(|e| eyre::eyre!("open keyring entry: {e}"))?;
    match entry.get_password() {
        Ok(json) => {
            let tokens: StoredTokens =
                serde_json::from_str(&json).map_err(|e| eyre::eyre!("parse stored tokens: {e}"))?;
            Ok(Some(tokens))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(eyre::eyre!("read tokens from keyring: {e}")),
    }
}

/// Delete stored tokens for a server (used by `octos mcp logout`).
pub fn delete_tokens(url: &str) -> Result<bool> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_key(url))
        .map_err(|e| eyre::eyre!("open keyring entry: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(eyre::eyre!("delete tokens from keyring: {e}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenStorageKind {
    Keyring,
    EncryptedFile,
}

fn scoped_keyring_key(profile_id: &str, url: &str) -> String {
    format!("{profile_id}|{}", keyring_key(url))
}

fn encrypted_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp-oauth")
}

fn encrypted_token_path(data_dir: &Path, url: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    encrypted_dir(data_dir).join(format!("{digest:x}.json"))
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_or_create_encryption_key(data_dir: &Path) -> Result<chacha20poly1305::Key> {
    let path = encrypted_dir(data_dir).join("key");
    if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            eyre::bail!("invalid MCP OAuth encryption key length");
        }
        return Ok(*chacha20poly1305::Key::from_slice(&bytes));
    }
    let key = XChaCha20Poly1305::generate_key(&mut OsRng);
    write_private_file(&path, key.as_slice())?;
    Ok(key)
}

fn save_encrypted_tokens(data_dir: &Path, url: &str, tokens: &StoredTokens) -> Result<()> {
    let key = read_or_create_encryption_key(data_dir)?;
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let plaintext = serde_json::to_vec(tokens)?;
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: url.as_bytes(),
            },
        )
        .map_err(|_| eyre::eyre!("failed to encrypt MCP OAuth tokens"))?;
    let body = serde_json::json!({
        "nonce": BASE64.encode(nonce),
        "ciphertext": BASE64.encode(ciphertext),
    });
    write_private_file(
        &encrypted_token_path(data_dir, url),
        body.to_string().as_bytes(),
    )
}

fn load_encrypted_tokens(data_dir: &Path, url: &str) -> Result<Option<StoredTokens>> {
    let path = encrypted_token_path(data_dir, url);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let nonce = BASE64.decode(body["nonce"].as_str().unwrap_or_default())?;
    if nonce.len() != 24 {
        eyre::bail!("invalid MCP OAuth token nonce");
    }
    let ciphertext = BASE64.decode(body["ciphertext"].as_str().unwrap_or_default())?;
    let key = read_or_create_encryption_key(data_dir)?;
    let cipher = XChaCha20Poly1305::new(&key);
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: url.as_bytes(),
            },
        )
        .map_err(|_| eyre::eyre!("failed to decrypt MCP OAuth tokens"))?;
    Ok(Some(serde_json::from_slice(&plaintext)?))
}

/// Report which backend a new scoped credential would use, without writing it.
pub fn token_storage_kind(profile_id: &str, url: &str) -> TokenStorageKind {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &scoped_keyring_key(profile_id, url))
    else {
        return TokenStorageKind::EncryptedFile;
    };
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => TokenStorageKind::Keyring,
        Err(_) => TokenStorageKind::EncryptedFile,
    }
}

/// Save tokens for one profile, preferring the OS keyring and falling back to
/// an authenticated-encryption file when the platform store is unavailable.
pub fn save_tokens_scoped(
    profile_id: &str,
    data_dir: &Path,
    url: &str,
    tokens: &StoredTokens,
) -> Result<TokenStorageKind> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &scoped_keyring_key(profile_id, url))
        .map_err(|error| eyre::eyre!("open keyring entry: {error}"))?;
    let json = serde_json::to_string(tokens)?;
    match entry.set_password(&json) {
        Ok(()) => Ok(TokenStorageKind::Keyring),
        Err(_) => {
            save_encrypted_tokens(data_dir, url, tokens)?;
            Ok(TokenStorageKind::EncryptedFile)
        }
    }
}

/// Load tokens for one profile. A missing keyring entry also checks the
/// encrypted fallback; no other profile's keyring entry is consulted.
pub fn load_tokens_scoped(
    profile_id: &str,
    data_dir: &Path,
    url: &str,
) -> Result<Option<(StoredTokens, TokenStorageKind)>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &scoped_keyring_key(profile_id, url))
        .map_err(|error| eyre::eyre!("open keyring entry: {error}"))?;
    match entry.get_password() {
        Ok(json) => Ok(Some((
            serde_json::from_str(&json)?,
            TokenStorageKind::Keyring,
        ))),
        Err(keyring::Error::NoEntry) => Ok(load_encrypted_tokens(data_dir, url)?
            .map(|tokens| (tokens, TokenStorageKind::EncryptedFile))),
        Err(_) => Ok(load_encrypted_tokens(data_dir, url)?
            .map(|tokens| (tokens, TokenStorageKind::EncryptedFile))),
    }
}

/// Delete a profile's tokens from both stores. Returns the backend used by the
/// operation for status/audit, without exposing any token material.
pub fn delete_tokens_scoped(
    profile_id: &str,
    data_dir: &Path,
    url: &str,
) -> Result<TokenStorageKind> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &scoped_keyring_key(profile_id, url))
        .map_err(|error| eyre::eyre!("open keyring entry: {error}"))?;
    let keyring_ok = entry.delete_credential().is_ok();
    let path = encrypted_token_path(data_dir, url);
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(if keyring_ok {
        TokenStorageKind::Keyring
    } else {
        TokenStorageKind::EncryptedFile
    })
}

/// Connect to an OAuth-gated streamable-HTTP MCP server using keyring-stored
/// tokens. rmcp's `AuthClient` refreshes the access token as needed.
pub async fn connect_oauth(
    config: &McpServerConfig,
    url: &str,
    client_info: ClientInfo,
) -> Result<McpService> {
    connect_oauth_inner(config, url, client_info, None, None).await
}

/// Profile-scoped OAuth connection used by ProfileRuntime. Token reads and
/// refreshes are isolated by profile and can use the encrypted fallback.
pub async fn connect_oauth_scoped(
    config: &McpServerConfig,
    url: &str,
    client_info: ClientInfo,
    profile_id: &str,
    data_dir: &Path,
) -> Result<McpService> {
    connect_oauth_inner(config, url, client_info, Some(profile_id), Some(data_dir)).await
}

async fn connect_oauth_inner(
    config: &McpServerConfig,
    url: &str,
    client_info: ClientInfo,
    profile_id: Option<&str>,
    data_dir: Option<&Path>,
) -> Result<McpService> {
    let stored = match (profile_id, data_dir) {
        (Some(profile_id), Some(data_dir)) => {
            load_tokens_scoped(profile_id, data_dir, url)?.map(|(tokens, _)| tokens)
        }
        _ => load_tokens(url)?,
    }
    .ok_or_else(|| {
        eyre::eyre!(
            "no stored OAuth tokens for MCP server '{url}'; run `octos mcp login {url}` first"
        )
    })?;

    let token: OAuthTokenResponse = serde_json::from_value(stored.token_response.clone())
        .map_err(|e| eyre::eyre!("parse stored oauth token: {e}"))?;

    // OAuth sends bearer tokens; require TLS and refuse a literal
    // private/loopback host up front (the DNS resolver is skipped for literals).
    require_https(url)?;
    crate::mcp::reject_private_url_host(url)?;

    // Transport client keeps the configured headers. OAuth operations
    // (discovery/registration/token/refresh) run through SsrfOAuthHttpClient,
    // which SSRF-validates EVERY endpoint URL (incl. literal-IP ones the DNS
    // resolver skips) and carries no resource-server headers/secret — those
    // requests can target a *different* authorization-server host.
    let transport_client = crate::mcp::build_ssrf_http_client(&config.headers)?;
    let oauth_http = std::sync::Arc::new(crate::mcp::SsrfOAuthHttpClient::new()?)
        as std::sync::Arc<dyn rmcp::transport::auth::OAuthHttpClient>;

    let mut oauth_state = timeout(
        HANDSHAKE_TIMEOUT,
        OAuthState::new_with_oauth_http_client(url.to_string(), oauth_http),
    )
    .await
    .map_err(|_| eyre::eyre!("oauth metadata discovery for '{url}' timed out"))?
    .map_err(|e| eyre::eyre!("oauth init for '{url}': {e}"))?;
    oauth_state
        .set_credentials(&stored.client_id, token)
        .await
        .map_err(|e| eyre::eyre!("load oauth credentials: {e}"))?;

    let manager = match oauth_state {
        OAuthState::Authorized(m) | OAuthState::Unauthorized(m) => m,
        _ => eyre::bail!("unexpected OAuth state after loading credentials"),
    };

    // `set_credentials` resets rmcp's token receipt time to now, so rmcp's
    // auto-refresh timer would be wrong (it assumes a full `expires_in` from
    // startup) and it would keep sending the token past its real expiry. If we
    // have a persisted expiry, refresh once on load so the receipt time is
    // correct — falling back to the stored token if refresh fails but it isn't
    // actually expired yet.
    if stored.expires_at_ms.is_some() {
        match manager.refresh_token().await {
            Ok(_) => persist_manager_credentials(url, &manager, profile_id, data_dir).await,
            Err(e) if token_expired(stored.expires_at_ms) => {
                return Err(eyre::eyre!(
                    "stored oauth token for '{url}' expired and refresh failed: {e}"
                ));
            }
            Err(e) => {
                tracing::warn!(server = url, error = %e, "oauth token refresh on load failed; using stored token (may expire mid-session)");
            }
        }
    }

    let auth_client = AuthClient::new(transport_client, manager);
    let transport = StreamableHttpClientTransport::with_client(
        auth_client,
        StreamableHttpClientTransportConfig::with_uri(url.to_string()),
    );

    let service = timeout(HANDSHAKE_TIMEOUT, serve_client(client_info, transport))
        .await
        .map_err(|_| eyre::eyre!("MCP OAuth handshake timed out after {HANDSHAKE_TIMEOUT:?}"))?
        .map_err(|e| eyre::eyre!("MCP OAuth initialize failed: {e}"))?;
    Ok(Arc::new(service))
}

/// Build the SSRF-validating OAuth HTTP client used by both CLI login and OUP
/// authorization flows. Every metadata/registration/token endpoint is checked.
pub fn new_oauth_http_client() -> Result<Arc<dyn rmcp::transport::auth::OAuthHttpClient>> {
    Ok(Arc::new(crate::mcp::SsrfOAuthHttpClient::new()?))
}

/// Run the interactive OAuth authorization-code flow for a server and persist
/// the resulting tokens. Opens the consent URL in a browser and catches the
/// redirect on an ephemeral localhost port.
pub async fn login(url: &str, scopes: &[String]) -> Result<()> {
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();

    require_https(url)?;
    crate::mcp::reject_private_url_host(url)?;
    // Route metadata discovery / dynamic registration / token exchange through
    // the SSRF-validating OAuth client so none of those endpoints can be pointed
    // at private/metadata addresses (incl. literal IPs the resolver skips).
    let oauth_http = std::sync::Arc::new(crate::mcp::SsrfOAuthHttpClient::new()?)
        as std::sync::Arc<dyn rmcp::transport::auth::OAuthHttpClient>;
    let mut oauth_state = timeout(
        HANDSHAKE_TIMEOUT,
        OAuthState::new_with_oauth_http_client(url.to_string(), oauth_http),
    )
    .await
    .map_err(|_| eyre::eyre!("oauth metadata discovery for '{url}' timed out"))?
    .map_err(|e| eyre::eyre!("oauth init for '{url}': {e}"))?;

    // Loopback catcher on an ephemeral localhost port.
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| eyre::eyre!("start loopback callback server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| eyre::eyre!("loopback address has no port"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    timeout(
        HANDSHAKE_TIMEOUT,
        oauth_state.start_authorization(&scope_refs, &redirect_uri, Some("octos")),
    )
    .await
    .map_err(|_| eyre::eyre!("oauth authorization/registration for '{url}' timed out"))?
    .map_err(|e| {
        eyre::eyre!("start authorization (metadata discovery / client registration): {e}")
    })?;
    let auth_url = oauth_state
        .get_authorization_url()
        .await
        .map_err(|e| eyre::eyre!("build authorization url: {e}"))?;

    println!("Opening your browser to authorize octos for {url} ...");
    println!("If it does not open, visit this URL:\n  {auth_url}\n");
    let _ = webbrowser::open(&auth_url);

    // Await the redirect on a blocking worker. The deadline lives *inside*
    // `wait_for_callback` (via `recv_timeout`) so the blocking task always
    // terminates — a plain outer `timeout` would leave `recv()` wedged and the
    // runtime would hang on drop.
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let callback_url = tokio::task::spawn_blocking(move || wait_for_callback(&server, deadline))
        .await
        .map_err(|e| eyre::eyre!("callback task failed: {e}"))??;

    // Pass the full callback URL so rmcp also sees the RFC 9207 `iss` parameter
    // (some authorization servers require it or the exchange fails).
    oauth_state
        .handle_callback_url(&callback_url)
        .await
        .map_err(|e| eyre::eyre!("exchange authorization code for tokens: {e}"))?;

    let (client_id, creds) = oauth_state
        .get_credentials()
        .await
        .map_err(|e| eyre::eyre!("read credentials after callback: {e}"))?;
    let creds =
        creds.ok_or_else(|| eyre::eyre!("authorization succeeded but no credentials returned"))?;

    let stored = StoredTokens {
        url: url.to_string(),
        client_id,
        token_response: serde_json::to_value(&creds)
            .map_err(|e| eyre::eyre!("serialize token response: {e}"))?,
        expires_at_ms: expires_at_ms(&creds),
    };
    save_tokens(url, &stored)?;
    println!("\u{2713} Authorized. Tokens stored in the OS keyring (\"{KEYRING_SERVICE}\").");
    Ok(())
}

/// Wall-clock expiry (ms) from the token's `expires_in`, if present.
fn expires_at_ms(creds: &OAuthTokenResponse) -> Option<u64> {
    use oauth2::TokenResponse;
    let dur = creds.expires_in()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((now + dur).as_millis() as u64)
}

/// Require `https://` for OAuth URLs — bearer tokens must not travel over
/// cleartext. (Loopback, where `http` is common for dev, is already refused by
/// `reject_private_url_host`, so this doesn't block a legitimate local case.)
fn require_https(url: &str) -> Result<()> {
    if !url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        eyre::bail!(
            "OAuth MCP server '{url}' must use https:// (refusing to send bearer tokens over cleartext)"
        );
    }
    Ok(())
}

/// Whether a stored token is at/near its persisted wall-clock expiry (30s skew).
/// `None` (server sent no `expires_in`) → treat as non-expiring; rmcp handles it.
fn token_expired(expires_at_ms: Option<u64>) -> bool {
    let Some(exp) = expires_at_ms else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_add(30_000) >= exp
}

/// Re-read the (possibly just-refreshed) credentials from the manager and
/// persist them to the keyring. Best-effort — a persist failure is logged, not
/// fatal (the in-memory token still works for this session).
async fn persist_manager_credentials(
    url: &str,
    manager: &AuthorizationManager,
    profile_id: Option<&str>,
    data_dir: Option<&Path>,
) {
    if let Ok((client_id, Some(creds))) = manager.get_credentials().await
        && let Ok(value) = serde_json::to_value(&creds)
    {
        let stored = StoredTokens {
            url: url.to_string(),
            client_id,
            token_response: value,
            expires_at_ms: expires_at_ms(&creds),
        };
        let result = match (profile_id, data_dir) {
            (Some(profile_id), Some(data_dir)) => {
                save_tokens_scoped(profile_id, data_dir, url, &stored).map(|_| ())
            }
            _ => save_tokens(url, &stored),
        };
        if let Err(e) = result {
            tracing::warn!(server = url, error = %e, "failed to persist refreshed MCP oauth token");
        }
    }
}

/// Block until the OAuth redirect hits the loopback server (or `deadline`);
/// return the FULL callback URL (so the caller can forward `code`/`state`/`iss`
/// to rmcp). Uses `recv_timeout` so the caller can bound the wait without
/// leaving a wedged blocking task.
fn wait_for_callback(server: &tiny_http::Server, deadline: Instant) -> Result<String> {
    loop {
        if Instant::now() >= deadline {
            eyre::bail!("timed out waiting for authorization (>{LOGIN_TIMEOUT:?})");
        }
        let req = match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(r)) => r,
            Ok(None) => continue, // tick — re-check the deadline
            Err(e) => eyre::bail!("receive callback request: {e}"),
        };
        // `req.url()` is just the path+query; wrap it so `url` can parse it.
        let full = format!("http://127.0.0.1{}", req.url());
        let parsed = url::Url::parse(&full).map_err(|e| eyre::eyre!("parse callback url: {e}"))?;

        let (mut has_code, mut has_state, mut err) = (false, false, None);
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => has_code = true,
                "state" => has_state = true,
                "error" => err = Some(v.into_owned()),
                _ => {}
            }
        }

        if let Some(e) = err {
            let _ = req.respond(tiny_http::Response::from_string(format!(
                "Authorization failed: {e}. You can close this tab."
            )));
            eyre::bail!("authorization denied by provider: {e}");
        }
        if has_code && has_state {
            let _ = req.respond(tiny_http::Response::from_string(
                "octos authorization complete \u{2014} you can close this tab.",
            ));
            // Return the full URL (incl. any `iss`) for rmcp to parse.
            return Ok(full);
        }
        // Stray request (e.g. favicon) — 404 and keep waiting.
        let _ = req.respond(
            tiny_http::Response::from_string("waiting for authorization callback")
                .with_status_code(404),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyring_key_is_stable_and_normalized() {
        let a = keyring_key("https://Example.com/mcp/");
        let b = keyring_key("https://example.com/mcp");
        assert_eq!(a, b, "trailing slash + case must normalize equal");
        assert!(a.starts_with("https://example.com/mcp|"));
    }

    #[test]
    fn keyring_key_differs_per_url() {
        assert_ne!(
            keyring_key("https://a.example/mcp"),
            keyring_key("https://b.example/mcp")
        );
    }

    #[test]
    fn encrypted_fallback_round_trips_without_plaintext_or_cross_profile_reads() {
        let first = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let url = "https://example.com/mcp";
        let tokens = StoredTokens {
            url: url.into(),
            client_id: "client-1".into(),
            token_response: serde_json::json!({"access_token": "secret-token"}),
            expires_at_ms: Some(123),
        };
        save_encrypted_tokens(first.path(), url, &tokens).unwrap();
        let path = encrypted_token_path(first.path(), url);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("secret-token"));
        let loaded = load_encrypted_tokens(first.path(), url)
            .unwrap()
            .expect("tokens must decrypt");
        assert_eq!(loaded.token_response["access_token"], "secret-token");
        assert!(load_encrypted_tokens(other.path(), url).unwrap().is_none());
        assert_ne!(
            scoped_keyring_key("profile-a", url),
            scoped_keyring_key("profile-b", url)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let key = encrypted_dir(first.path()).join("key");
            assert_eq!(
                std::fs::metadata(key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn keyring_key_preserves_case_sensitive_path() {
        // Scheme/host are case-insensitive (normalize equal), but paths are not
        // — /MCP and /mcp must map to distinct keyring entries.
        assert_eq!(
            keyring_key("HTTPS://Host.Example/mcp"),
            keyring_key("https://host.example/mcp")
        );
        assert_ne!(
            keyring_key("https://host.example/MCP"),
            keyring_key("https://host.example/mcp")
        );
    }
}
