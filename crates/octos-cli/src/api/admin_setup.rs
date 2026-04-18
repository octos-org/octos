//! First-run admin setup endpoints.
//!
//! Exposes the status handler that backs the dashboard `BootstrapGate`.
//! All routes live under `/api/admin/...` and are gated by the admin auth
//! middleware.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use super::AppState;

#[derive(Serialize)]
pub struct TokenStatus {
    pub rotated: bool,
}

/// GET `/api/admin/token/status` — whether the bootstrap token has been
/// rotated into a hashed persistent record.
pub async fn token_status(State(state): State<Arc<AppState>>) -> Json<TokenStatus> {
    Json(TokenStatus {
        rotated: state.admin_token_store.exists(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_token_store::{AdminTokenRecord, AdminTokenStore};
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
}
