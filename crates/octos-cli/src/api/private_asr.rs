//! Authenticated exchange from an Octos user session to a one-time private-ASR grant.
//!
//! The long-lived service credential stays in the Octos server environment. The
//! browser receives only the short-lived grant returned by the ASR control plane.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::router::AuthIdentity;

const CONTROL_URL_ENV: &str = "PRIVATE_ASR_CONTROL_URL";
const SERVICE_TOKEN_ENV: &str = "PRIVATE_ASR_SERVICE_TOKEN";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq, Eq)]
struct PrivateAsrConfig {
    grant_url: String,
    readiness_url: String,
    service_token: String,
}

impl fmt::Debug for PrivateAsrConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateAsrConfig")
            .field("grant_url", &self.grant_url)
            .field("readiness_url", &self.readiness_url)
            .field("service_token", &"[REDACTED]")
            .finish()
    }
}

impl PrivateAsrConfig {
    fn from_env() -> Result<Self, PrivateAsrError> {
        Self::from_values(
            std::env::var(CONTROL_URL_ENV).ok().as_deref(),
            std::env::var(SERVICE_TOKEN_ENV).ok().as_deref(),
        )
    }

    fn from_values(
        control_url: Option<&str>,
        service_token: Option<&str>,
    ) -> Result<Self, PrivateAsrError> {
        let control_url = control_url.map(str::trim).filter(|value| !value.is_empty());
        let service_token = service_token
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let (Some(control_url), Some(service_token)) = (control_url, service_token) else {
            return Err(PrivateAsrError::Unconfigured);
        };
        let parsed = reqwest::Url::parse(control_url).map_err(|_| PrivateAsrError::Unconfigured)?;
        if !control_url_is_secure(&parsed)
            || parsed.host_str().is_none()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || service_token.len() < 24
        {
            return Err(PrivateAsrError::Unconfigured);
        }
        let base = control_url.trim_end_matches('/');
        Ok(Self {
            grant_url: format!("{base}/api/v1/browser-grants"),
            readiness_url: format!("{base}/readyz"),
            service_token: service_token.to_owned(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrivateAsrReadiness {
    pub ready: bool,
    pub detail: String,
}

/// Probe the browser-facing private ASR route when it is configured.
///
/// `None` means that neither private-ASR setting is present, so callers should
/// retain the normal local-development route (`ASR_API_URL` or OMiniX). Once an
/// operator supplies either setting, an invalid pair is reported as not ready
/// instead of silently falling back to an unrelated ASR implementation.
pub(crate) async fn readiness(client: &reqwest::Client) -> Option<PrivateAsrReadiness> {
    let control_url = std::env::var(CONTROL_URL_ENV).ok();
    let service_token = std::env::var(SERVICE_TOKEN_ENV).ok();
    readiness_from_values(client, control_url.as_deref(), service_token.as_deref()).await
}

async fn readiness_from_values(
    client: &reqwest::Client,
    control_url: Option<&str>,
    service_token: Option<&str>,
) -> Option<PrivateAsrReadiness> {
    let configured = control_url.is_some_and(|value| !value.trim().is_empty())
        || service_token.is_some_and(|value| !value.trim().is_empty());
    if !configured {
        return None;
    }
    let config = match PrivateAsrConfig::from_values(control_url, service_token) {
        Ok(config) => config,
        Err(_) => {
            return Some(PrivateAsrReadiness {
                ready: false,
                detail: "Private ASR configuration is incomplete or invalid".into(),
            });
        }
    };
    let response = client
        .get(&config.readiness_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    Some(match response {
        Ok(response) if response.status().is_success() => PrivateAsrReadiness {
            ready: true,
            detail: "Private ASR ready".into(),
        },
        Ok(response) => PrivateAsrReadiness {
            ready: false,
            detail: format!(
                "Private ASR readiness check returned HTTP {}",
                response.status()
            ),
        },
        Err(error) => PrivateAsrReadiness {
            ready: false,
            detail: if error.is_timeout() {
                "Private ASR readiness check timed out".into()
            } else {
                "Private ASR is unreachable".into()
            },
        },
    })
}

fn control_url_is_secure(url: &reqwest::Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    if url.scheme() != "http" {
        return false;
    }
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserGrantResponse {
    grant: String,
    expires_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamGrantResponse {
    grant: String,
    expires_at_ms: u64,
}

#[derive(Debug, Serialize)]
struct UpstreamGrantRequest<'a> {
    subject: &'a str,
    #[serde(rename = "profileId")]
    profile_id: &'a str,
}

#[derive(Debug, Serialize)]
pub(crate) struct PrivateAsrErrorBody {
    error: PrivateAsrErrorEnvelope,
}

#[derive(Debug, Serialize)]
struct PrivateAsrErrorEnvelope {
    code: &'static str,
    message: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateAsrError {
    Unconfigured,
    Forbidden,
    Unavailable,
    InvalidResponse,
}

impl PrivateAsrError {
    fn response(self) -> (StatusCode, Json<PrivateAsrErrorBody>) {
        let (status, code, message) = match self {
            Self::Unconfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "private_asr_unconfigured",
                "Private speech recognition is not configured",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "private_asr_forbidden",
                "Private speech recognition is not available for this profile",
            ),
            Self::Unavailable => (
                StatusCode::BAD_GATEWAY,
                "private_asr_unavailable",
                "Private speech recognition is temporarily unavailable",
            ),
            Self::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "private_asr_invalid_response",
                "Private speech recognition returned an invalid response",
            ),
        };
        (
            status,
            Json(PrivateAsrErrorBody {
                error: PrivateAsrErrorEnvelope { code, message },
            }),
        )
    }
}

fn validate_upstream_grant(
    response: UpstreamGrantResponse,
    now_ms: u64,
) -> Result<BrowserGrantResponse, PrivateAsrError> {
    if response.grant.is_empty()
        || response.grant.len() > 512
        || response.grant.chars().any(char::is_whitespace)
        || response.expires_at_ms <= now_ms
    {
        return Err(PrivateAsrError::InvalidResponse);
    }
    Ok(BrowserGrantResponse {
        grant: response.grant,
        expires_at_ms: response.expires_at_ms,
    })
}

async fn exchange_browser_grant(
    client: &reqwest::Client,
    config: &PrivateAsrConfig,
    subject: &str,
    profile_id: &str,
) -> Result<BrowserGrantResponse, PrivateAsrError> {
    let response = client
        .post(&config.grant_url)
        .bearer_auth(&config.service_token)
        .timeout(REQUEST_TIMEOUT)
        .json(&UpstreamGrantRequest {
            subject,
            profile_id,
        })
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "private ASR grant exchange failed");
            PrivateAsrError::Unavailable
        })?;

    if !response.status().is_success() {
        tracing::warn!(
            upstream_status = %response.status(),
            "private ASR grant exchange was rejected"
        );
        return Err(PrivateAsrError::Unavailable);
    }
    let upstream = response
        .json::<UpstreamGrantResponse>()
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "private ASR grant response could not be decoded");
            PrivateAsrError::InvalidResponse
        })?;
    validate_upstream_grant(upstream, unix_ms())
}

pub(crate) async fn browser_grant(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Json<BrowserGrantResponse>, (StatusCode, Json<PrivateAsrErrorBody>)> {
    let config = PrivateAsrConfig::from_env().map_err(PrivateAsrError::response)?;
    let profile_store = state
        .profile_store
        .as_ref()
        .ok_or_else(|| PrivateAsrError::Unconfigured.response())?;
    let profile_id =
        super::auth_handlers::resolve_my_profile_id(&identity, profile_store, &state, &headers)
            .map_err(|status| {
                if status == StatusCode::FORBIDDEN {
                    PrivateAsrError::Forbidden.response()
                } else {
                    PrivateAsrError::Unconfigured.response()
                }
            })?;
    exchange_browser_grant(&state.http_client, &config, &profile_id, &profile_id)
        .await
        .map(Json)
        .map_err(PrivateAsrError::response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn config_builds_only_the_fixed_grant_endpoint() {
        let config = PrivateAsrConfig::from_values(Some("https://asr.example.com/"), Some(TOKEN))
            .expect("valid config");
        assert_eq!(
            config.grant_url,
            "https://asr.example.com/api/v1/browser-grants"
        );
        assert_eq!(config.readiness_url, "https://asr.example.com/readyz");
        assert_eq!(config.service_token, TOKEN);
    }

    #[test]
    fn config_rejects_missing_short_or_credentialed_values() {
        assert_eq!(
            PrivateAsrConfig::from_values(None, Some(TOKEN)),
            Err(PrivateAsrError::Unconfigured)
        );
        assert_eq!(
            PrivateAsrConfig::from_values(Some("https://asr.example.com"), Some("short")),
            Err(PrivateAsrError::Unconfigured)
        );
        assert_eq!(
            PrivateAsrConfig::from_values(Some("https://user@asr.example.com"), Some(TOKEN)),
            Err(PrivateAsrError::Unconfigured)
        );
        assert_eq!(
            PrivateAsrConfig::from_values(Some("http://asr.example.com"), Some(TOKEN)),
            Err(PrivateAsrError::Unconfigured)
        );
        assert!(PrivateAsrConfig::from_values(Some("http://127.0.0.1:8080"), Some(TOKEN)).is_ok());
    }

    #[test]
    fn config_debug_output_redacts_the_service_token() {
        let config = PrivateAsrConfig::from_values(Some("https://asr.example.com"), Some(TOKEN))
            .expect("valid config");
        let output = format!("{config:?}");
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains(TOKEN));
    }

    #[test]
    fn upstream_grant_is_strictly_validated() {
        let valid = validate_upstream_grant(
            UpstreamGrantResponse {
                grant: "temporary-grant".into(),
                expires_at_ms: 42,
            },
            41,
        )
        .expect("valid grant");
        assert_eq!(valid.grant, "temporary-grant");

        assert_eq!(
            validate_upstream_grant(
                UpstreamGrantResponse {
                    grant: "contains whitespace".into(),
                    expires_at_ms: 42,
                },
                41,
            ),
            Err(PrivateAsrError::InvalidResponse)
        );
        assert_eq!(
            validate_upstream_grant(
                UpstreamGrantResponse {
                    grant: "expired-grant".into(),
                    expires_at_ms: 42,
                },
                42,
            ),
            Err(PrivateAsrError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn exchange_uses_service_bearer_and_authenticated_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/browser-grants"))
            .and(header("authorization", format!("Bearer {TOKEN}")))
            .and(body_json(serde_json::json!({
                "subject": "user-1",
                "profileId": "profile-1",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "grant": "one-time-grant",
                "expiresAtMs": u64::MAX,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let config = PrivateAsrConfig::from_values(Some(&server.uri()), Some(TOKEN))
            .expect("valid test config");

        let grant = exchange_browser_grant(&reqwest::Client::new(), &config, "user-1", "profile-1")
            .await
            .expect("exchange succeeds");
        assert_eq!(grant.grant, "one-time-grant");
        assert_eq!(grant.expires_at_ms, u64::MAX);
    }

    #[tokio::test]
    async fn readiness_is_absent_when_private_asr_is_not_configured() {
        assert_eq!(
            readiness_from_values(&reqwest::Client::new(), None, None).await,
            None
        );
    }

    #[tokio::test]
    async fn readiness_rejects_an_incomplete_private_asr_configuration() {
        assert_eq!(
            readiness_from_values(
                &reqwest::Client::new(),
                Some("https://asr.example.com"),
                None,
            )
            .await,
            Some(PrivateAsrReadiness {
                ready: false,
                detail: "Private ASR configuration is incomplete or invalid".into(),
            })
        );
    }

    #[tokio::test]
    async fn readiness_probes_the_private_asr_ready_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/readyz"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            readiness_from_values(&reqwest::Client::new(), Some(&server.uri()), Some(TOKEN)).await,
            Some(PrivateAsrReadiness {
                ready: true,
                detail: "Private ASR ready".into(),
            })
        );
    }

    #[tokio::test]
    async fn readiness_reports_when_the_private_asr_is_not_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/readyz"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            readiness_from_values(&reqwest::Client::new(), Some(&server.uri()), Some(TOKEN)).await,
            Some(PrivateAsrReadiness {
                ready: false,
                detail: "Private ASR readiness check returned HTTP 503 Service Unavailable".into(),
            })
        );
    }
}
