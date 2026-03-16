//! OAuth PKCE and device code flows for OpenAI.

use std::collections::HashMap;

use chrono::Utc;
use eyre::{Result, WrapErr};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::store::AuthCredential;

// OpenAI OAuth configuration (public client — same as picoclaw/claude-code).
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_SCOPES: &str = "openid profile email offline_access";
const REDIRECT_PORT: u16 = 1455;
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
// Pre-encoded for URL query string construction.
const REDIRECT_URI_ENCODED: &str = "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
const OPENAI_SCOPES_ENCODED: &str = "openid%20profile%20email%20offline_access";

struct PkceChallenge {
    verifier: String,
    challenge: String,
}

/// Generate a PKCE code verifier and S256 challenge.
fn generate_pkce() -> PkceChallenge {
    // 64 random hex chars from two UUIDv4s (sufficient entropy for PKCE).
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().as_simple(),
        uuid::Uuid::new_v4().as_simple(),
    );

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64_url_encode(&hash);

    PkceChallenge {
        verifier,
        challenge,
    }
}

/// Base64-URL encode without padding (RFC 7636).
fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Generate a random state parameter.
fn random_state() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pkce_verifier_length() {
        let pkce = generate_pkce();
        // Two UUIDv4 simple strings = 32+32 = 64 hex chars
        assert_eq!(pkce.verifier.len(), 64);
    }

    #[test]
    fn test_generate_pkce_challenge_is_base64url() {
        let pkce = generate_pkce();
        // S256 challenge should be non-empty base64url (no padding)
        assert!(!pkce.challenge.is_empty());
        assert!(!pkce.challenge.contains('='));
        assert!(!pkce.challenge.contains('+'));
        assert!(!pkce.challenge.contains('/'));
        // SHA-256 output = 32 bytes -> base64url = 43 chars (without padding)
        assert_eq!(pkce.challenge.len(), 43);
    }

    #[test]
    fn test_generate_pkce_unique() {
        let p1 = generate_pkce();
        let p2 = generate_pkce();
        assert_ne!(p1.verifier, p2.verifier);
        assert_ne!(p1.challenge, p2.challenge);
    }

    #[test]
    fn test_base64_url_encode_known_value() {
        // SHA-256 of empty string
        let hash = sha2::Sha256::digest(b"");
        let encoded = base64_url_encode(&hash);
        assert_eq!(encoded, "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU");
    }

    #[test]
    fn test_base64_url_encode_no_padding() {
        let encoded = base64_url_encode(b"test");
        assert!(!encoded.contains('='));
    }

    #[test]
    fn test_random_state_format() {
        let state = random_state();
        // UUIDv4 simple = 32 hex chars
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_random_state_unique() {
        let s1 = random_state();
        let s2 = random_state();
        assert_ne!(s1, s2);
    }
}

/// Run the browser-based OAuth PKCE flow for OpenAI.
pub async fn browser_oauth_flow() -> Result<AuthCredential> {
    let pkce = generate_pkce();
    let state = random_state();

    let auth_url = format!(
        "{}/authorize?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        OPENAI_ISSUER,
        OPENAI_CLIENT_ID,
        REDIRECT_URI_ENCODED,
        OPENAI_SCOPES_ENCODED,
        pkce.challenge,
        state,
    );

    // Start listener before opening browser to avoid race.
    let listener = TcpListener::bind(format!("127.0.0.1:{REDIRECT_PORT}"))
        .await
        .wrap_err_with(|| format!("failed to bind port {REDIRECT_PORT} for OAuth callback"))?;

    println!("Opening browser for OpenAI login...");
    if open::that(&auth_url).is_err() {
        println!("Could not open browser. Please visit:\n{auth_url}");
    }

    // Wait for the callback.
    let code = wait_for_callback(&listener, &state).await?;

    // Exchange code for token.
    let token = exchange_code(&code, &pkce.verifier).await?;

    Ok(token_to_credential(token, "oauth"))
}

/// Run the device code OAuth flow for OpenAI.
pub async fn device_code_flow() -> Result<AuthCredential> {
    let client = Client::new();

    let resp = client
        .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/usercode"))
        .form(&[("client_id", OPENAI_CLIENT_ID), ("scope", OPENAI_SCOPES)])
        .send()
        .await
        .wrap_err("failed to request device code")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("device code request failed: {body}");
    }

    let device: DeviceCodeResponse = resp.json().await?;

    println!();
    println!("Go to: {}", device.verification_uri);
    println!("Enter code: {}", device.user_code);
    println!();
    println!("Waiting for authorization...");

    let interval = std::time::Duration::from_secs(device.interval.max(5));
    let deadline = Utc::now() + chrono::Duration::seconds(device.expires_in as i64);

    loop {
        tokio::time::sleep(interval).await;

        if Utc::now() > deadline {
            eyre::bail!("device code authorization timed out");
        }

        let resp = client
            .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/token"))
            .form(&[
                ("client_id", OPENAI_CLIENT_ID),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        if resp.status().is_success() {
            let token: TokenResponse = resp.json().await?;
            return Ok(token_to_credential(token, "device_code"));
        }

        // Check for "authorization_pending" vs actual error.
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let error = body["error"].as_str().unwrap_or("");
        match error {
            "authorization_pending" | "slow_down" => continue,
            _ => eyre::bail!("device code auth failed: {}", body),
        }
    }
}

/// Wait for OAuth callback on the local TCP listener.
async fn wait_for_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .wrap_err("failed to accept callback connection")?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse: GET /auth/callback?code=XXX&state=YYY HTTP/1.1
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| eyre::eyre!("invalid callback request"))?;

    let query = path
        .split('?')
        .nth(1)
        .ok_or_else(|| eyre::eyre!("no query string in callback"))?;

    let params: HashMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k, v))
        })
        .collect();

    let state = params
        .get("state")
        .ok_or_else(|| eyre::eyre!("no state in callback"))?;
    if *state != expected_state {
        eyre::bail!("OAuth state mismatch — possible CSRF attack");
    }

    let code = params
        .get("code")
        .ok_or_else(|| eyre::eyre!("no code in callback"))?
        .to_string();

    // Send HTML response back to browser.
    let html = "<html><body><h2>Login successful!</h2><p>You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    stream.write_all(response.as_bytes()).await.ok();

    Ok(code)
}

/// Exchange authorization code for tokens.
async fn exchange_code(code: &str, verifier: &str) -> Result<TokenResponse> {
    let client = Client::new();
    let resp = client
        .post(format!("{OPENAI_ISSUER}/api/accounts/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", OPENAI_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .wrap_err("failed to exchange code for token")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("token exchange failed: {body}");
    }

    resp.json().await.wrap_err("failed to parse token response")
}

fn token_to_credential(token: TokenResponse, auth_method: &str) -> AuthCredential {
    let expires_at = token
        .expires_in
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs as i64));

    AuthCredential {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
        provider: "openai".to_string(),
        auth_method: auth_method.to_string(),
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}
