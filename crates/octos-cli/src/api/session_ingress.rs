//! External CLI-agent session ingress over WebSocket.

use std::sync::Arc;

use axum::extract::ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use octos_agent::bridge::work_secret::WorkSecretValidationError;
use octos_core::SessionKey;

use super::AppState;

pub(crate) async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some((token, source)) = extract_session_ingress_token(&headers, &uri) else {
        let message = if query_has_removed_token_alias(&uri) {
            "credential query parameter removed; send Authorization: Bearer <token> or ?token="
        } else {
            "missing session ingress token"
        };
        return (axum::http::StatusCode::UNAUTHORIZED, message).into_response();
    };

    let session_id = SessionKey(session_id);
    let grant = match state.work_secret_store.validate(&session_id.0, &token) {
        Ok(grant) => grant,
        Err(error) => {
            let (status, message) = match error {
                WorkSecretValidationError::Missing => {
                    (axum::http::StatusCode::UNAUTHORIZED, "invalid token")
                }
                WorkSecretValidationError::SessionMismatch => {
                    (axum::http::StatusCode::FORBIDDEN, "session mismatch")
                }
                WorkSecretValidationError::Expired => {
                    (axum::http::StatusCode::UNAUTHORIZED, "token expired")
                }
                WorkSecretValidationError::Revoked => {
                    (axum::http::StatusCode::UNAUTHORIZED, "token revoked")
                }
            };
            return (status, message).into_response();
        }
    };
    // Warned only once the grant validates: unauthenticated scanners fuzzing
    // `?token=` must not be able to flood the log with this line.
    if source == IngressTokenSource::QueryParam {
        tracing::warn!(
            "session ingress credential carried in the deprecated `?token=` query \
             parameter, which exposes it to request-line logging in intermediaries; \
             send `Authorization: Bearer <token>` instead"
        );
    }

    super::ui_protocol_transport::ws_handler_for_session_ingress(
        state,
        session_id,
        grant.profile_id,
        token,
        headers,
        uri,
        ws,
    )
    .await
}

/// Where the session ingress credential was carried on the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngressTokenSource {
    AuthorizationHeader,
    /// Deprecated `?token=` fallback for WebSocket clients that cannot set
    /// headers; its use is logged so operators can migrate to the header.
    QueryParam,
}

/// Extract the work secret, preferring the `Authorization` header.
///
/// `?token=` remains only as a deprecated fallback for WebSocket clients that
/// cannot set headers; the former `_token` / `session_ingress_token` query
/// aliases were removed (#2370) because a bearer credential in the request
/// line leaks into intermediary access logs.
fn extract_session_ingress_token(
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<(String, IngressTokenSource)> {
    if let Some(header_token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
    {
        return Some((
            header_token.to_owned(),
            IngressTokenSource::AuthorizationHeader,
        ));
    }
    let query_token = uri.query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
    })?;
    let token = percent_encoding::percent_decode_str(query_token)
        .decode_utf8_lossy()
        .into_owned();
    (!token.is_empty()).then_some((token, IngressTokenSource::QueryParam))
}

/// True when the credential arrived under a removed (`_token` /
/// `session_ingress_token`) query alias (#2370). Used only to give stale
/// clients a diagnostic 401 message instead of a bare "missing" one.
fn query_has_removed_token_alias(uri: &Uri) -> bool {
    uri.query().is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair.starts_with("_token=") || pair.starts_with("session_ingress_token="))
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, Uri};

    use super::{IngressTokenSource, extract_session_ingress_token, query_has_removed_token_alias};

    #[test]
    fn extracts_bearer_token_before_query_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer header-token".parse().unwrap());
        let uri: Uri = "/v1/session_ingress/ws/s?token=query-token"
            .parse()
            .unwrap();
        assert_eq!(
            extract_session_ingress_token(&headers, &uri),
            Some((
                "header-token".to_owned(),
                IngressTokenSource::AuthorizationHeader
            ))
        );
    }

    #[test]
    fn extracts_percent_decoded_query_token() {
        let headers = HeaderMap::new();
        let uri: Uri = "/v1/session_ingress/ws/s?token=A%2FB%3DC".parse().unwrap();
        assert_eq!(
            extract_session_ingress_token(&headers, &uri),
            Some(("A/B=C".to_owned(), IngressTokenSource::QueryParam))
        );
    }

    #[test]
    fn rejects_removed_query_token_aliases() {
        let headers = HeaderMap::new();
        for alias in ["_token", "session_ingress_token"] {
            let uri: Uri = format!("/v1/session_ingress/ws/s?{alias}=secret")
                .parse()
                .unwrap();
            assert_eq!(extract_session_ingress_token(&headers, &uri), None);
        }
    }

    #[test]
    fn detects_removed_query_token_aliases_for_diagnostics() {
        for alias in ["_token", "session_ingress_token"] {
            let uri: Uri = format!("/v1/session_ingress/ws/s?{alias}=secret")
                .parse()
                .unwrap();
            assert!(query_has_removed_token_alias(&uri));
        }
        for kept in ["token", "not_token", "xsession_ingress_token"] {
            let uri: Uri = format!("/v1/session_ingress/ws/s?{kept}=secret")
                .parse()
                .unwrap();
            assert!(!query_has_removed_token_alias(&uri));
        }
        let no_query: Uri = "/v1/session_ingress/ws/s".parse().unwrap();
        assert!(!query_has_removed_token_alias(&no_query));
    }

    #[test]
    fn rejects_missing_and_empty_tokens() {
        let headers = HeaderMap::new();
        let bare: Uri = "/v1/session_ingress/ws/s".parse().unwrap();
        assert_eq!(extract_session_ingress_token(&headers, &bare), None);
        let empty: Uri = "/v1/session_ingress/ws/s?token=".parse().unwrap();
        assert_eq!(extract_session_ingress_token(&headers, &empty), None);
        let mut empty_header = HeaderMap::new();
        empty_header.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(extract_session_ingress_token(&empty_header, &bare), None);
    }
}
