//! No-password "solo" login for local single-user installs.
//!
//! In `DeploymentMode::Local` the dashboard is a single-operator tool on a
//! loopback-only host, so forcing an admin-token / OTP login adds friction
//! with no security benefit. These endpoints mirror the TUI's solo
//! onboarding (`profile/local/create` is the login primitive): the SPA can
//! create a local profile and/or obtain a session token without a password.
//!
//! ## Security model — fail closed
//!
//! Both handlers gate on [`solo_login_allowed`], which requires BOTH:
//!   1. [`supports_local_solo_profile_create`] — `deployment_mode == Local`
//!      with profile + user stores present, AND
//!   2. a loopback request peer (`ConnectInfo` IP `is_loopback()`).
//!
//! Local mode binds `127.0.0.1` and runs no reverse proxy (proxies are a
//! tenant/cloud concern), so a loopback peer here is a genuine local client
//! — the same loopback-⇒-trusted model the codebase already uses for
//! `X-Profile-Id` (`router::is_trusted_proxy_addr`). Tenant/cloud hosts,
//! which DO terminate proxies over loopback, are excluded by gate (1), so
//! the proxy-spoofing path can never reach these handlers.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Json, State};
use axum::http::StatusCode;
use serde::Serialize;

use octos_core::ui_protocol::{
    ProfileLocalCreateParams, ProfileLocalCreateResult, RpcError, rpc_error_codes,
};

use super::AppState;
use super::auth_handlers::is_top_level_profile_id;
use super::ui_protocol::{create_or_get_local_solo_profile, supports_local_solo_profile_create};
use crate::user_store::{User, UserRole};

/// `POST /api/auth/solo/create` response: the created/looked-up local
/// profile plus a freshly minted session token. Flattened so the SPA sees
/// the same shape it would from `profile/local/create`, with `token` added.
#[derive(Debug, Serialize)]
pub struct SoloCreateResponse {
    #[serde(flatten)]
    pub result: ProfileLocalCreateResult,
    pub token: String,
}

/// `POST /api/auth/solo` response: a session token for the existing local
/// solo owner.
#[derive(Debug, Serialize)]
pub struct SoloLoginResponse {
    pub token: String,
    pub user: User,
}

/// The one security-critical check in the solo path. See the module docs.
/// Returns `Ok(())` only for a Local-solo host reached over loopback;
/// everything else is `403`.
fn solo_login_allowed(state: &AppState, remote_ip: Option<IpAddr>) -> Result<(), StatusCode> {
    if !supports_local_solo_profile_create(state) {
        return Err(StatusCode::FORBIDDEN);
    }
    match remote_ip {
        Some(ip) if ip.is_loopback() => Ok(()),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

/// Map the RPC error from the shared profile-creation logic onto an HTTP
/// status. Validation failures are the caller's fault (400); the unsupported
/// gate is a policy refusal (403); anything else is a server fault (500).
fn rpc_error_to_status(err: &RpcError) -> StatusCode {
    match err.code {
        rpc_error_codes::INVALID_PARAMS => StatusCode::BAD_REQUEST,
        rpc_error_codes::PERMISSION_DENIED => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Resolve the local solo owner. In solo mode there is normally exactly one
/// top-level user; if several exist (e.g. repeated `solo/create` with
/// different usernames) the most recently created wins so a fresh onboard
/// takes effect. Sub-accounts are excluded.
fn resolve_solo_user(state: &AppState) -> Option<User> {
    let store = state.user_store.as_ref()?;
    let mut users: Vec<User> = store.list().ok()?;
    users.retain(|u| is_top_level_profile_id(state, &u.id));
    users.into_iter().max_by_key(|u| u.created_at)
}

/// Mint a session token for `user_id`. `validate_session` re-reads the live
/// role from the store, so `role` here is non-authoritative.
async fn mint_solo_session(
    state: &AppState,
    user_id: &str,
    role: UserRole,
) -> Result<String, StatusCode> {
    let auth_manager = state
        .auth_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    auth_manager
        .create_session_for_user(user_id, role)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /api/auth/solo/create` — onboard a local profile AND log in.
///
/// Public route (no auth middleware): the whole point is that no credential
/// exists yet. Gated at request time by [`solo_login_allowed`].
pub async fn solo_create(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(params): Json<ProfileLocalCreateParams>,
) -> Result<Json<SoloCreateResponse>, StatusCode> {
    solo_login_allowed(&state, Some(addr.ip()))?;
    let result = create_or_get_local_solo_profile(&state, params)
        .map_err(|err| rpc_error_to_status(&err))?;
    // The local solo owner is created with the Admin role
    // (see `create_or_get_local_solo_profile`).
    let token = mint_solo_session(&state, &result.user_id, UserRole::Admin).await?;
    Ok(Json(SoloCreateResponse { result, token }))
}

/// `POST /api/auth/solo` — re-login for the existing local solo owner.
///
/// `404` when no solo profile exists yet (the SPA then shows the create
/// form). Gated by [`solo_login_allowed`].
pub async fn solo_login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<SoloLoginResponse>, StatusCode> {
    solo_login_allowed(&state, Some(addr.ip()))?;
    let user = resolve_solo_user(&state).ok_or(StatusCode::NOT_FOUND)?;
    let token = mint_solo_session(&state, &user.id, user.role.clone()).await?;
    Ok(Json(SoloLoginResponse { token, user }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeploymentMode;
    use crate::otp::AuthManager;
    use crate::profiles::ProfileStore;
    use crate::user_store::UserStore;
    use std::path::Path;

    fn solo_state_in_mode(dir: &Path, mode: DeploymentMode) -> Arc<AppState> {
        let user_store = Arc::new(UserStore::open(dir).unwrap());
        let auth_manager = Arc::new(AuthManager::new(None, user_store.clone()));
        Arc::new(AppState {
            profile_store: Some(Arc::new(ProfileStore::open(dir).unwrap())),
            user_store: Some(user_store),
            auth_manager: Some(auth_manager),
            deployment_mode: mode,
            ..AppState::empty_for_tests()
        })
    }

    fn solo_state(dir: &Path) -> Arc<AppState> {
        solo_state_in_mode(dir, DeploymentMode::Local)
    }

    fn loopback() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000)))
    }

    fn remote() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([8, 8, 8, 8], 40000)))
    }

    fn params(name: &str, username: &str, email: &str) -> ProfileLocalCreateParams {
        ProfileLocalCreateParams {
            name: name.into(),
            username: username.into(),
            email: email.into(),
        }
    }

    #[tokio::test]
    async fn solo_create_returns_profile_and_token_when_local_and_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let Json(resp) = solo_create(
            State(state.clone()),
            loopback(),
            Json(params("Ada Lovelace", "ada", "ada@example.com")),
        )
        .await
        .expect("solo create should succeed on a local loopback host");

        assert!(resp.result.created, "first create should report created");
        assert_eq!(resp.result.runtime_mode, "solo");
        assert_eq!(resp.result.user_id, "ada");
        assert!(!resp.token.is_empty());

        // The minted token must validate through the normal session path.
        let mgr = state.auth_manager.as_ref().unwrap();
        let (user_id, role) = mgr.validate_session(&resp.token).await.unwrap();
        assert_eq!(user_id, "ada");
        assert_eq!(role, UserRole::Admin);
    }

    #[tokio::test]
    async fn solo_login_returns_token_when_profile_exists() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        // Seed via create, then re-login.
        let _ = solo_create(
            State(state.clone()),
            loopback(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();

        let Json(resp) = solo_login(State(state.clone()), loopback())
            .await
            .expect("solo login should succeed once a profile exists");

        assert_eq!(resp.user.id, "ada");
        let mgr = state.auth_manager.as_ref().unwrap();
        let (user_id, _) = mgr.validate_session(&resp.token).await.unwrap();
        assert_eq!(user_id, "ada");
    }

    #[tokio::test]
    async fn solo_login_404_when_no_profile_yet() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let err = solo_login(State(state), loopback())
            .await
            .expect_err("no solo profile yet → 404 so the SPA shows the create form");
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn solo_create_403_when_not_local_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state_in_mode(dir.path(), DeploymentMode::Tenant);

        let err = solo_create(
            State(state),
            loopback(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .expect_err("solo create must be refused on a tenant host");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_create_403_when_peer_not_loopback() {
        // Security: even in Local mode, a non-loopback peer must be refused.
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let err = solo_create(
            State(state),
            remote(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .expect_err("non-loopback peer must be refused");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_login_403_when_peer_not_loopback() {
        // Security: a fully-valid would-be login is still blocked off-loopback,
        // proving the guard (not a missing profile) is what refuses it.
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());
        let _ = solo_create(
            State(state.clone()),
            loopback(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();

        let err = solo_login(State(state), remote())
            .await
            .expect_err("non-loopback peer must be refused even with a valid profile");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_create_400_on_invalid_username() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let err = solo_create(
            State(state),
            loopback(),
            Json(params("Ada", "has space", "ada@example.com")),
        )
        .await
        .expect_err("an invalid username must surface as a 400");
        assert_eq!(err, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn solo_create_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let Json(first) = solo_create(
            State(state.clone()),
            loopback(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();
        assert!(first.result.created);

        let Json(second) = solo_create(
            State(state.clone()),
            loopback(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();
        assert!(
            !second.result.created,
            "re-running create for the same owner is a no-op create"
        );
    }
}
