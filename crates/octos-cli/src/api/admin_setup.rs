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
use crate::setup_state_store::SetupState;

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

#[derive(Deserialize)]
pub struct StepBody {
    pub step: u32,
}

/// GET `/api/admin/setup/state` — current wizard state (completion, skip,
/// last step reached). Returns a default (all-empty) state when no file
/// exists yet.
pub async fn get_setup_state(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SetupState>, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.load().map(Json).map_err(|e| {
        tracing::error!(error = ?e, "failed to load setup state");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "load_failed",
                message: e.to_string(),
            }),
        )
    })
}

/// POST `/api/admin/setup/step` — record the furthest wizard step reached.
/// Rejects `step > 5` with `invalid_step`.
pub async fn post_setup_step(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StepBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    if body.step > 5 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_step",
                message: "step must be between 0 and 5 inclusive".into(),
            }),
        ));
    }
    state
        .setup_state_store
        .update_last_step(body.step)
        .map_err(|e| {
            tracing::error!(error = ?e, "failed to persist setup step");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "save_failed",
                    message: e.to_string(),
                }),
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST `/api/admin/setup/complete` — mark the wizard as completed.
pub async fn post_setup_complete(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.mark_complete().map_err(|e| {
        tracing::error!(error = ?e, "failed to mark setup complete");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST `/api/admin/setup/skip` — mark the wizard as skipped.
pub async fn post_setup_skip(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.mark_skipped().map_err(|e| {
        tracing::error!(error = ?e, "failed to mark setup skipped");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
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
    use crate::setup_state_store::SetupStateStore;

    fn state_with_store(dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            admin_token_store: Arc::new(AdminTokenStore::new(dir)),
            setup_state_store: Arc::new(SetupStateStore::new(dir)),
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
    async fn get_setup_state_returns_default_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let Json(s) = get_setup_state(State(state)).await.unwrap();
        assert!(s.wizard_completed_at.is_none());
        assert!(!s.wizard_skipped);
        assert_eq!(s.wizard_last_step_reached, 0);
    }

    #[tokio::test]
    async fn post_setup_step_persists_last_step() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_step(State(state.clone()), Json(StepBody { step: 3 }))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let loaded = state.setup_state_store.load().unwrap();
        assert_eq!(loaded.wizard_last_step_reached, 3);
    }

    #[tokio::test]
    async fn post_setup_step_accepts_boundary_values() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        for step in [0u32, 5u32] {
            let status = post_setup_step(State(state.clone()), Json(StepBody { step }))
                .await
                .unwrap();
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(
                state
                    .setup_state_store
                    .load()
                    .unwrap()
                    .wizard_last_step_reached,
                step
            );
        }
    }

    #[tokio::test]
    async fn post_setup_step_rejects_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let err = post_setup_step(State(state), Json(StepBody { step: 6 }))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_step");
    }

    #[tokio::test]
    async fn post_setup_complete_marks_completed_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_complete(State(state.clone())).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.setup_state_store.load().unwrap();
        assert!(s.wizard_completed_at.is_some());
        assert!(!s.wizard_skipped);
    }

    #[tokio::test]
    async fn post_setup_skip_marks_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_skip(State(state.clone())).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.setup_state_store.load().unwrap();
        assert!(s.wizard_completed_at.is_some());
        assert!(s.wizard_skipped);
    }

    #[tokio::test]
    async fn get_setup_state_round_trips_via_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());

        post_setup_step(State(state.clone()), Json(StepBody { step: 4 }))
            .await
            .unwrap();
        post_setup_complete(State(state.clone())).await.unwrap();

        let Json(s) = get_setup_state(State(state)).await.unwrap();
        assert_eq!(s.wizard_last_step_reached, 4);
        assert!(s.wizard_completed_at.is_some());
        assert!(!s.wizard_skipped);
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
