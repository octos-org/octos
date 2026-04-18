//! First-run admin setup endpoints.
//!
//! Exposes the status + rotation handlers that back the dashboard
//! `BootstrapGate` and `SetupRotateToken` page. All routes live under
//! `/api/admin/...` and are gated by the admin auth middleware.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::admin_token_store::AdminTokenRecord;

#[derive(Serialize)]
pub struct TokenStatus {
    pub rotated: bool,
}

#[derive(Deserialize)]
pub struct RotateBody {
    pub new_token: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

/// GET `/api/admin/token/status` — whether the bootstrap token has been
/// rotated into a hashed persistent record.
pub async fn token_status(State(state): State<Arc<AppState>>) -> Json<TokenStatus> {
    Json(TokenStatus {
        rotated: state.admin_token_store.exists(),
    })
}

/// POST `/api/admin/token/rotate` — replace the bootstrap token with a
/// hashed persistent record. Refuses if a record already exists (operator
/// must `octos admin reset-token` to restart).
pub async fn rotate_token(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RotateBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    validate_token_strength(&body.new_token)?;
    if state.admin_token_store.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorBody {
                code: "already_rotated",
                message:
                    "admin token has already been rotated; use `octos admin reset-token` to start over"
                        .into(),
            }),
        ));
    }
    let record = AdminTokenRecord::from_plaintext(&body.new_token);
    state.admin_token_store.save(&record).map_err(|e| {
        tracing::error!(error = ?e, "failed to persist rotated admin token");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    tracing::info!("admin token rotated via dashboard");
    Ok(StatusCode::NO_CONTENT)
}

fn validate_token_strength(t: &str) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    if t.len() < 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "weak_token",
                message: "token must be at least 32 characters".into(),
            }),
        ));
    }
    let mut classes = 0;
    if t.chars().any(|c| c.is_ascii_lowercase()) {
        classes += 1;
    }
    if t.chars().any(|c| c.is_ascii_uppercase()) {
        classes += 1;
    }
    if t.chars().any(|c| c.is_ascii_digit()) {
        classes += 1;
    }
    if t.chars().any(|c| !c.is_ascii_alphanumeric()) {
        classes += 1;
    }
    if classes < 3 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "weak_token",
                message: "token must contain at least 3 of: lowercase, uppercase, digits, symbols"
                    .into(),
            }),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_token_store::AdminTokenStore;
    use crate::api::AppState;

    fn state_with_store(dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            admin_token_store: Arc::new(AdminTokenStore::new(dir)),
            ..AppState::empty_for_tests()
        })
    }

    #[tokio::test]
    async fn token_status_is_false_before_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let Json(status) = token_status(State(state)).await;
        assert!(!status.rotated);
    }

    #[tokio::test]
    async fn token_status_is_true_after_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "Already-Rotated-Token-1234567890-ABCDE",
            ))
            .unwrap();
        let state = state_with_store(dir.path());
        let Json(status) = token_status(State(state)).await;
        assert!(status.rotated);
    }

    #[tokio::test]
    async fn rotate_token_persists_and_returns_204() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "Strong-New-Token-1234567890-ABCDEFGHI".into(),
        };
        let status = rotate_token(State(state.clone()), Json(body))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let record = state.admin_token_store.load().unwrap().unwrap();
        assert!(record.verify("Strong-New-Token-1234567890-ABCDEFGHI"));
    }

    #[tokio::test]
    async fn rotate_token_rejects_short_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "TooShort-1Aa".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "weak_token");
    }

    #[tokio::test]
    async fn rotate_token_rejects_low_entropy_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        // 32 chars but only lowercase + digit = 2 classes
        let body = RotateBody {
            new_token: "abcdefghijklmnopqrstuvwxyz012345".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "weak_token");
    }

    #[tokio::test]
    async fn rotate_token_conflicts_when_already_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "Already-Rotated-Token-1234567890-ABCDE",
            ))
            .unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "Another-Strong-Token-1234567890-ABCDEFGH".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.code, "already_rotated");
    }
}
