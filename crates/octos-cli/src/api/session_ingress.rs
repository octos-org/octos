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
    let token = extract_session_ingress_token(&headers, &uri);
    if token.is_empty() {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "missing session ingress token",
        )
            .into_response();
    }

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

    super::ui_protocol::ws_handler_for_session_ingress(
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

fn extract_session_ingress_token(headers: &HeaderMap, uri: &Uri) -> String {
    let header_token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if !header_token.is_empty() {
        return header_token.to_owned();
    }
    let query_token = uri
        .query()
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                pair.strip_prefix("token=")
                    .or_else(|| pair.strip_prefix("_token="))
                    .or_else(|| pair.strip_prefix("session_ingress_token="))
            })
        })
        .unwrap_or("");
    percent_encoding::percent_decode_str(query_token)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, Uri};

    use super::extract_session_ingress_token;

    #[test]
    fn extracts_bearer_token_before_query_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer header-token".parse().unwrap());
        let uri: Uri = "/v1/session_ingress/ws/s?token=query-token"
            .parse()
            .unwrap();
        assert_eq!(
            extract_session_ingress_token(&headers, &uri),
            "header-token"
        );
    }

    #[test]
    fn extracts_percent_decoded_query_token() {
        let headers = HeaderMap::new();
        let uri: Uri = "/v1/session_ingress/ws/s?session_ingress_token=A%2FB%3DC"
            .parse()
            .unwrap();
        assert_eq!(extract_session_ingress_token(&headers, &uri), "A/B=C");
    }
}
