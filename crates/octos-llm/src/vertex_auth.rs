//! Vertex AI service-account auth.
//!
//! Google Vertex AI does not accept a plain API key like AI Studio. It uses
//! OAuth2 Bearer tokens minted from a service-account JSON key:
//!
//! 1. Parse the SA JSON (`client_email`, `private_key`, `project_id`, `token_uri`).
//! 2. Sign a short-lived JWT assertion (RS256) with the SA private key.
//! 3. Exchange the assertion at `token_uri` for an `access_token` (~1h TTL).
//! 4. Cache the token and refresh shortly before expiry.
//!
//! [`GeminiProvider`](crate::gemini::GeminiProvider) holds a
//! [`TokenSource`] (typically a [`VertexTokenProvider`]) and attaches
//! `Authorization: Bearer <token>` to each request when in Vertex mode.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// OAuth2 scope required for Vertex AI generateContent.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// JWT assertion lifetime (Google caps this at 1h).
const ASSERTION_TTL_SECS: u64 = 3600;
/// Refresh this long before actual expiry so a token is never used right as it
/// lapses (clock skew + in-flight request slack).
const EXPIRY_SKEW: Duration = Duration::from_secs(60);
/// Google's default token endpoint, used when the SA JSON omits `token_uri`.
fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

/// A Google service account, parsed from its JSON key file.
#[derive(Clone)]
pub struct ServiceAccount {
    pub client_email: String,
    pub private_key: SecretString,
    pub project_id: String,
    pub token_uri: String,
}

#[derive(Deserialize)]
struct RawServiceAccount {
    client_email: Option<String>,
    private_key: Option<String>,
    project_id: Option<String>,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

impl ServiceAccount {
    /// Parse a service account from its JSON contents.
    pub fn from_json(s: &str) -> Result<Self> {
        let raw: RawServiceAccount =
            serde_json::from_str(s).wrap_err("invalid service account JSON")?;
        Ok(Self {
            client_email: raw
                .client_email
                .ok_or_else(|| eyre::eyre!("service account JSON missing 'client_email'"))?,
            private_key: SecretString::from(
                raw.private_key
                    .ok_or_else(|| eyre::eyre!("service account JSON missing 'private_key'"))?,
            ),
            project_id: raw
                .project_id
                .ok_or_else(|| eyre::eyre!("service account JSON missing 'project_id'"))?,
            token_uri: raw.token_uri,
        })
    }

    /// Read and parse a service account from a JSON file on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read service account JSON: {}", path.display()))?;
        Self::from_json(&data)
    }
}

/// Current unix time in seconds (wall clock).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the JWT claim set for a service-account token exchange.
///
/// Split out from signing so the claim shape is unit-testable without a real
/// RSA key.
fn build_claims(sa: &ServiceAccount, now_unix: u64) -> serde_json::Value {
    serde_json::json!({
        "iss": sa.client_email,
        "scope": SCOPE,
        "aud": sa.token_uri,
        "iat": now_unix,
        "exp": now_unix + ASSERTION_TTL_SECS,
    })
}

/// Sign a JWT assertion (RS256) for the service account.
fn build_assertion(sa: &ServiceAccount, now_unix: u64) -> Result<String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

    let claims = build_claims(sa, now_unix);
    let key = EncodingKey::from_rsa_pem(sa.private_key.expose_secret().as_bytes())
        .wrap_err("invalid RSA private_key in service account")?;
    encode(&Header::new(Algorithm::RS256), &claims, &key).wrap_err("failed to sign JWT assertion")
}

/// Something that yields a Vertex AI bearer token (cached + auto-refreshing).
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// Return a currently-valid access token.
    async fn token(&self) -> Result<String>;
}

/// Fetches a fresh `(token, ttl)` from the token endpoint. Split from caching so
/// the cache logic can be tested with a fake fetcher (no network, no signing).
#[async_trait]
trait TokenFetch: Send + Sync {
    async fn fetch(&self) -> Result<(String, Duration)>;
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}
fn default_expires_in() -> u64 {
    ASSERTION_TTL_SECS
}

/// Real fetcher: sign an assertion and exchange it at Google's token endpoint.
struct GoogleJwtFetch {
    sa: ServiceAccount,
    client: reqwest::Client,
}

#[async_trait]
impl TokenFetch for GoogleJwtFetch {
    async fn fetch(&self) -> Result<(String, Duration)> {
        let assertion = build_assertion(&self.sa, now_unix())?;
        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];
        let resp = self
            .client
            .post(&self.sa.token_uri)
            .form(&params)
            .send()
            .await
            .wrap_err("failed to request Vertex access token")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("Vertex token endpoint returned {status}: {body}");
        }
        let tr: TokenResponse = resp
            .json()
            .await
            .wrap_err("invalid Vertex token response body")?;
        Ok((tr.access_token, Duration::from_secs(tr.expires_in)))
    }
}

struct Cached {
    value: String,
    expires_at: Instant,
}

/// Caching, auto-refreshing Vertex token provider.
pub struct VertexTokenProvider {
    fetcher: Box<dyn TokenFetch>,
    cache: Mutex<Option<Cached>>,
    skew: Duration,
}

impl VertexTokenProvider {
    /// Build a provider that signs + exchanges tokens for the given service
    /// account, using the default LLM HTTP timeouts for the token-exchange call.
    pub fn from_service_account(sa: ServiceAccount) -> Self {
        Self::from_service_account_with_timeout(
            sa,
            crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
        )
    }

    /// Like [`from_service_account`], but bounds the token-exchange HTTP client
    /// with the given timeouts so a stalled OAuth endpoint can't hang a chat
    /// past the configured LLM timeout before `generateContent` is reached.
    pub fn from_service_account_with_timeout(
        sa: ServiceAccount,
        timeout_secs: u64,
        connect_timeout_secs: u64,
    ) -> Self {
        Self {
            fetcher: Box::new(GoogleJwtFetch {
                sa,
                client: crate::provider::build_http_client(timeout_secs, connect_timeout_secs),
            }),
            cache: Mutex::new(None),
            skew: EXPIRY_SKEW,
        }
    }

    #[cfg(test)]
    fn with_fetcher(fetcher: Box<dyn TokenFetch>) -> Self {
        Self {
            fetcher,
            cache: Mutex::new(None),
            skew: EXPIRY_SKEW,
        }
    }
}

#[async_trait]
impl TokenSource for VertexTokenProvider {
    async fn token(&self) -> Result<String> {
        // Fast path: a cached token with comfortable remaining lifetime.
        {
            let guard = self.cache.lock().expect("token cache poisoned");
            if let Some(c) = guard.as_ref() {
                if c.expires_at.saturating_duration_since(Instant::now()) > self.skew {
                    return Ok(c.value.clone());
                }
            }
        }
        // Slow path: refresh. (A rare concurrent double-fetch is harmless.)
        let (value, ttl) = self.fetcher.fetch().await?;
        let expires_at = Instant::now() + ttl;
        let mut guard = self.cache.lock().expect("token cache poisoned");
        *guard = Some(Cached {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_sa_json() -> &'static str {
        r#"{
            "type": "service_account",
            "project_id": "my-proj",
            "client_email": "svc@my-proj.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\nNOTAREALKEY\n-----END PRIVATE KEY-----\n",
            "token_uri": "https://oauth2.googleapis.com/token"
        }"#
    }

    #[test]
    fn should_parse_service_account_fields_when_valid_json() {
        let sa = ServiceAccount::from_json(sample_sa_json()).unwrap();
        assert_eq!(sa.project_id, "my-proj");
        assert_eq!(sa.client_email, "svc@my-proj.iam.gserviceaccount.com");
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
        assert!(sa.private_key.expose_secret().contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn should_default_token_uri_when_absent() {
        let json = r#"{"project_id":"p","client_email":"e","private_key":"k"}"#;
        let sa = ServiceAccount::from_json(json).unwrap();
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn should_error_when_private_key_missing() {
        let json = r#"{"project_id":"p","client_email":"e"}"#;
        let err = ServiceAccount::from_json(json)
            .err()
            .expect("missing private_key should error")
            .to_string();
        assert!(err.contains("private_key"), "got: {err}");
    }

    #[test]
    fn should_build_jwt_claims_with_expected_fields_when_signing() {
        let sa = ServiceAccount::from_json(sample_sa_json()).unwrap();
        let claims = build_claims(&sa, 1_000);
        assert_eq!(claims["iss"], "svc@my-proj.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["iat"], 1_000);
        assert_eq!(claims["exp"], 1_000 + 3600);
    }

    // A pinned, test-only RSA keypair generated for this suite (never a real
    // credential). Guards the #2764 review finding: jsonwebtoken 10 links no
    // RS256 backend unless a crypto feature is enabled, and `encode` panics
    // at runtime — no prior test signed with a valid key, so the breakage
    // shipped green. This test fails (panic) on a backend-less build.
    const PINNED_TEST_RSA_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCkxPv87cQlcZbl
6rgPrwTFA2UHerRSllVTpJgahzOSGjUwrq7Uzn+CN6sHZ3YG3zfRIugLywsQOx09
jzhyOPbFklIBZHZ3LyUnmCtdPQH+CNj/WOyEnmEerRuMhniJJimR3k6wMHdwB//r
DZVqCGfckGBj2kAlH4pFVbmHFIIxvsqPyuwPIiuoCEN7FH+0F0sXO/i8ropqZfN9
qjTlJ9c3TEzspibGxQQifYm0xxtek7aOArFV+sXJLryxIBeU2k+5y62MiXW4S3gD
H4OqJFuCbKmLR/P+N73CHZn+M/xcXJQj9/oMgW+IHmE40LdO1r1LKYl3rV0EDe6k
qOwUp12dAgMBAAECggEAUHBUXsHK6Ma/9sW0jv+QEghauaCHSOxYgqmPkh/gJtsB
jZ8x/fVD3ZQEvGwffDJcQw7J/vvkjkt1tnhlg6nYm1KYE1Ly3LOY7XAF95JRly3F
lj9Y1FOnsTysftmlHbuSPsKrYqt+m6RkJWHvyCCLZ+vaCtVhTF5N+BRmKxJxjwu/
ddV6YJcAGuQUDt6KzVReUREfIiU/N1Nz17Ra4yHYOTo6i7qqNv8wpWxC1+gL8UUX
CUBWQ7nr6HSOIfc+5qUFvLZOJ/A3Zu3vfEjW0H0cG85JUqUMiKEQShay4YxIdUvK
VXRZq733ITjFl75i3cGF0IwK0KrnFaVyeBVRDhkLVwKBgQDYUIP2TrMtypINKHLF
CyGaln1JdWIukHFBmpSweopPMF3nV9VuZ3tr51WrBDDmngY0Gmoq3881XrlQlBKC
/GObhsEmUFV7Rrfg9dhdwibdvpeJHHMouV+OYOEY4vkbOkQvEgHiBWyJyNSTOV/E
fhis449WRvMaHnEnRU3v+QSqswKBgQDC/5c6Yw75Pbyh+USA9YgRx7exof2mfuIB
nZEHnXhh0u06/H/b100FqC/t2+tIKjsNjoV9ZkguEmp0qrLIK1dve1AuaPKf1LcB
O3u3YGhSI12kIMyAz4oezL0ppTZf5fmXrEMpf3gfn6je2T0ifIhpCQ4kGBTkEHGK
3aejCY4+bwKBgEdM8tJjSohTdyTDKmFss9zisdVezcgPcGVxcIVjPuevR6STvRQx
I+uDSyedsAi0GdNeyZPsRb6PxIGc7SUj0ClZl5zraeHCg6WxI2p0Lp90zToymHrZ
73dc05WCax3gbM0fIGPB6e7Ytcj5Zmm8LqH+OCb2J/d6BU4ud+wbpsAlAoGAcrIL
rZgecpPRpCQOGxxQkMS8U4tIuune3bxkVepVqJR8XTmvqwuUjkNDDdFRLv1IBs1R
z1SeGBtU6pKM3hhAMZ6CpwmrzD899UebmVznf4i1x5hTMWDhiQQ8L4ax2O5YRNJa
HsqPSWqL1ahJwUMmqUFeQ/S2NP9HCbseo1M3i5kCgYEAgilwYQ0/FAhAM/6ZK/wr
gJAxbjxYytArXbIyNnKRQ/aVZgy87NQqjA6sv5LKL3sKrG0jjsiiGtTu/M27yUYV
35ehk8GAN/Qfa5rhZBjemNbSByqRYbxveEomgiO1qnGzsegN9nKfitUJKxVEAdwv
Jj7ZKEH7AG5rwAawZuHjdHI=
-----END PRIVATE KEY-----"#;
    const PINNED_TEST_RSA_PUB: &str = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApMT7/O3EJXGW5eq4D68E
xQNlB3q0UpZVU6SYGoczkho1MK6u1M5/gjerB2d2Bt830SLoC8sLEDsdPY84cjj2
xZJSAWR2dy8lJ5grXT0B/gjY/1jshJ5hHq0bjIZ4iSYpkd5OsDB3cAf/6w2Vaghn
3JBgY9pAJR+KRVW5hxSCMb7Kj8rsDyIrqAhDexR/tBdLFzv4vK6KamXzfao05SfX
N0xM7KYmxsUEIn2JtMcbXpO2jgKxVfrFyS68sSAXlNpPucutjIl1uEt4Ax+DqiRb
gmypi0fz/je9wh2Z/jP8XFyUI/f6DIFviB5hONC3Tta9SymJd61dBA3upKjsFKdd
nQIDAQAB
-----END PUBLIC KEY-----"#;

    #[test]
    fn should_sign_and_verify_rs256_assertion_with_pinned_key() {
        use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

        let json = format!(
            r#"{{"type":"service_account","project_id":"my-proj","client_email":"svc@my-proj.iam.gserviceaccount.com","private_key":"{key}","token_uri":"https://oauth2.googleapis.com/token"}}"#,
            key = PINNED_TEST_RSA_KEY.replace(0x0A as char, "\\n"),
        );
        let sa = ServiceAccount::from_json(&json).unwrap();
        let token = build_assertion(&sa, 1_000)
            .expect("RS256 signing must succeed when a crypto backend is linked");
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
        validation.validate_aud = false;
        let decoded = decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_rsa_pem(PINNED_TEST_RSA_PUB.as_bytes()).unwrap(),
            &validation,
        )
        .expect("self-signed assertion must verify against the pinned public key");
        assert_eq!(decoded.claims["iss"], "svc@my-proj.iam.gserviceaccount.com");
        assert_eq!(decoded.claims["scope"], SCOPE);
        assert_eq!(decoded.header.alg, Algorithm::RS256);
    }

    #[test]
    fn should_error_when_private_key_invalid_pem() {
        // The signing path must surface a clear error rather than panic when the
        // SA carries a malformed private key.
        let sa = ServiceAccount::from_json(sample_sa_json()).unwrap();
        assert!(build_assertion(&sa, 1_000).is_err());
    }

    struct CountingFetch {
        count: AtomicUsize,
        ttl: Duration,
    }

    #[async_trait]
    impl TokenFetch for CountingFetch {
        async fn fetch(&self) -> Result<(String, Duration)> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            Ok((format!("tok{n}"), self.ttl))
        }
    }

    #[tokio::test]
    async fn should_cache_token_until_near_expiry() {
        let provider = VertexTokenProvider::with_fetcher(Box::new(CountingFetch {
            count: AtomicUsize::new(0),
            ttl: Duration::from_secs(3600),
        }));
        let a = provider.token().await.unwrap();
        let b = provider.token().await.unwrap();
        assert_eq!(a, "tok0");
        assert_eq!(b, "tok0", "second call should hit the cache");
    }

    #[tokio::test]
    async fn should_refresh_token_after_expiry() {
        // ttl=0 → the cached token is always within the skew window, so every
        // call must refetch a fresh token.
        let provider = VertexTokenProvider::with_fetcher(Box::new(CountingFetch {
            count: AtomicUsize::new(0),
            ttl: Duration::from_secs(0),
        }));
        let a = provider.token().await.unwrap();
        let b = provider.token().await.unwrap();
        assert_eq!(a, "tok0");
        assert_eq!(b, "tok1", "expired token should be refreshed");
    }
}
