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

use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use rmcp::model::ClientInfo;
use rmcp::service::serve_client;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::{AuthClient, OAuthState, OAuthTokenResponse};
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
pub fn keyring_key(url: &str) -> String {
    let normalized = url.trim_end_matches('/').to_ascii_lowercase();
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

/// Connect to an OAuth-gated streamable-HTTP MCP server using keyring-stored
/// tokens. rmcp's `AuthClient` refreshes the access token as needed.
pub async fn connect_oauth(
    _config: &McpServerConfig,
    url: &str,
    client_info: ClientInfo,
) -> Result<McpService> {
    let stored = load_tokens(url)?.ok_or_else(|| {
        eyre::eyre!("no stored OAuth tokens for MCP server '{url}'; run `octos mcp login {url}` first")
    })?;

    let token: OAuthTokenResponse = serde_json::from_value(stored.token_response.clone())
        .map_err(|e| eyre::eyre!("parse stored oauth token: {e}"))?;

    let mut oauth_state = OAuthState::new(url.to_string(), None)
        .await
        .map_err(|e| eyre::eyre!("oauth init for '{url}': {e}"))?;
    oauth_state
        .set_credentials(&stored.client_id, token)
        .await
        .map_err(|e| eyre::eyre!("load oauth credentials: {e}"))?;

    let manager = match oauth_state {
        OAuthState::Authorized(m) | OAuthState::Unauthorized(m) => m,
        _ => eyre::bail!("unexpected OAuth state after loading credentials"),
    };

    let auth_client = AuthClient::new(reqwest_rmcp::Client::new(), manager);
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

/// Run the interactive OAuth authorization-code flow for a server and persist
/// the resulting tokens. Opens the consent URL in a browser and catches the
/// redirect on an ephemeral localhost port.
pub async fn login(url: &str, scopes: &[String]) -> Result<()> {
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();

    let mut oauth_state = OAuthState::new(url.to_string(), None)
        .await
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

    oauth_state
        .start_authorization(&scope_refs, &redirect_uri, Some("octos"))
        .await
        .map_err(|e| eyre::eyre!("start authorization (metadata discovery / client registration): {e}"))?;
    let auth_url = oauth_state
        .get_authorization_url()
        .await
        .map_err(|e| eyre::eyre!("build authorization url: {e}"))?;

    println!("Opening your browser to authorize octos for {url} ...");
    println!("If it does not open, visit this URL:\n  {auth_url}\n");
    let _ = webbrowser::open(&auth_url);

    // Await the redirect on a blocking worker, bounded by LOGIN_TIMEOUT.
    let (code, state) = timeout(
        LOGIN_TIMEOUT,
        tokio::task::spawn_blocking(move || wait_for_callback(&server)),
    )
    .await
    .map_err(|_| eyre::eyre!("timed out waiting for authorization (>{LOGIN_TIMEOUT:?})"))?
    .map_err(|e| eyre::eyre!("callback task failed: {e}"))??;

    oauth_state
        .handle_callback(&code, &state)
        .await
        .map_err(|e| eyre::eyre!("exchange authorization code for tokens: {e}"))?;

    let (client_id, creds) = oauth_state
        .get_credentials()
        .await
        .map_err(|e| eyre::eyre!("read credentials after callback: {e}"))?;
    let creds = creds.ok_or_else(|| eyre::eyre!("authorization succeeded but no credentials returned"))?;

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

/// Block until the OAuth redirect hits the loopback server; return `(code, state)`.
fn wait_for_callback(server: &tiny_http::Server) -> Result<(String, String)> {
    loop {
        let req = server
            .recv()
            .map_err(|e| eyre::eyre!("receive callback request: {e}"))?;
        // `req.url()` is just the path+query; wrap it so `url` can parse it.
        let full = format!("http://localhost{}", req.url());
        let parsed =
            url::Url::parse(&full).map_err(|e| eyre::eyre!("parse callback url: {e}"))?;

        let (mut code, mut state, mut err) = (None, None, None);
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
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
        if let (Some(c), Some(s)) = (code, state) {
            let _ = req.respond(tiny_http::Response::from_string(
                "octos authorization complete \u{2014} you can close this tab.",
            ));
            return Ok((c, s));
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
}
