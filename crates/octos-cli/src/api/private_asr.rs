//! Authenticated exchange from an Octos user session to a one-time private-ASR grant.
//!
//! The long-lived service credential stays in the Octos server environment. The
//! browser receives only the short-lived grant returned by the ASR control plane.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::router::AuthIdentity;

const CONTROL_URL_ENV: &str = "PRIVATE_ASR_CONTROL_URL";
const SERVICE_TOKEN_ENV: &str = "PRIVATE_ASR_SERVICE_TOKEN";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PrivateAsrConfig {
    grant_url: String,
    service_token: String,
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
        if !matches!(parsed.scheme(), "http" | "https")
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
            service_token: service_token.to_owned(),
        })
    }
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

fn identity_scope(identity: &AuthIdentity) -> (&str, &str) {
    match identity {
        AuthIdentity::Admin => ("admin", "admin"),
        AuthIdentity::User { id, .. } => (id, id),
    }
}

fn validate_upstream_grant(
    response: UpstreamGrantResponse,
) -> Result<BrowserGrantResponse, PrivateAsrError> {
    if response.grant.is_empty()
        || response.grant.len() > 512
        || response.grant.chars().any(char::is_whitespace)
        || response.expires_at_ms == 0
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
    validate_upstream_grant(upstream)
}

pub(crate) async fn browser_grant(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Json<BrowserGrantResponse>, (StatusCode, Json<PrivateAsrErrorBody>)> {
    let config = PrivateAsrConfig::from_env().map_err(PrivateAsrError::response)?;
    let (subject, profile_id) = identity_scope(&identity);
    exchange_browser_grant(&state.http_client, &config, subject, profile_id)
        .await
        .map(Json)
        .map_err(PrivateAsrError::response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_store::UserRole;
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
    }

    #[test]
    fn authenticated_user_is_bound_to_their_own_profile() {
        let identity = AuthIdentity::User {
            id: "profile-1".into(),
            role: UserRole::User,
        };
        assert_eq!(identity_scope(&identity), ("profile-1", "profile-1"));
    }

    #[test]
    fn upstream_grant_is_strictly_validated() {
        let valid = validate_upstream_grant(UpstreamGrantResponse {
            grant: "temporary-grant".into(),
            expires_at_ms: 42,
        })
        .expect("valid grant");
        assert_eq!(valid.grant, "temporary-grant");

        assert_eq!(
            validate_upstream_grant(UpstreamGrantResponse {
                grant: "contains whitespace".into(),
                expires_at_ms: 42,
            }),
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
                "expiresAtMs": 1234,
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
        assert_eq!(grant.expires_at_ms, 1234);
    }
}
