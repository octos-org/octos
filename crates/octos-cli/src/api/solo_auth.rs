//! No-password "solo" login for local single-user installs.
//!
//! The dashboard normally needs an admin-token / OTP login. On a genuine
//! single-user local box that is pure friction, so these endpoints let the
//! SPA create a local profile and/or obtain a session token without a
//! password — mirroring the TUI's solo onboarding (`profile/local/create`
//! is the login primitive).
//!
//! ## Security model — fail closed
//!
//! Both handlers gate on [`solo_login_allowed`], which requires ALL of:
//!   1. [`supports_local_solo_profile_create`] — which itself requires the
//!      explicit operator **opt-in** (`octos serve --solo` /
//!      `OCTOS_SOLO_LOGIN=1`) AND `deployment_mode == Local` with profile +
//!      user stores. The opt-in lives on the shared predicate so it gates the
//!      WS `profile/local/create` path too, not just these REST endpoints.
//!   2. a loopback request peer (`ConnectInfo` IP `is_loopback()`), AND
//!   3. NO reverse-proxy headers on the request.
//!
//! The opt-in is the primary defence and exists because `deployment_mode ==
//! Local` is NOT a safe proxy for "single-user box". A hosted fleet daemon
//! runs Local mode behind a Caddy reverse proxy, so every external request
//! reaches the daemon over loopback and would pass gate (2) — the codebase
//! itself notes "loopback ⇒ trusted [proxy]" in `router::is_trusted_proxy_addr`.
//! The opt-in (which fleet configs never set) plus gate (3) — rejecting any
//! request that carries `X-Forwarded-*` / `X-Real-IP` / `Forwarded` — ensure a
//! proxied request can never launder itself through the loopback check.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Json, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;

use octos_core::ui_protocol::{
    ProfileLocalCreateParams, ProfileLocalCreateResult, RpcError, rpc_error_codes,
};

use super::AppState;
use super::auth_handlers::{is_login_ready_email, is_top_level_profile_id};
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

/// Headers a reverse proxy sets but a direct local client does not. Their
/// presence means the request was forwarded (e.g. by the Caddy that fronts
/// the fleet), so it must never satisfy the loopback check.
const PROXY_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "forwarded",
];

fn is_proxied(headers: &HeaderMap) -> bool {
    PROXY_HEADERS.iter().any(|h| headers.contains_key(*h))
}

/// The security gate for the solo path. See the module docs for the full
/// rationale. Returns `Ok(())` only when the operator opted in, the host is
/// Local with stores, the peer is loopback, AND the request is not proxied.
fn solo_login_allowed(
    state: &AppState,
    remote_ip: Option<IpAddr>,
    headers: &HeaderMap,
) -> Result<(), StatusCode> {
    // (1)+(2) Opt-in (folded into supports_local_solo_profile_create) AND
    // Local mode with profile/user stores. The opt-in is the primary defence
    // — a hosted fleet daemon never sets it.
    if !supports_local_solo_profile_create(state) {
        return Err(StatusCode::FORBIDDEN);
    }
    // (3) Loopback peer AND (4) not proxied — a forwarded request from Caddy
    // arrives over loopback but carries proxy headers; reject it.
    let loopback = remote_ip.map(|ip| ip.is_loopback()).unwrap_or(false);
    if !loopback || is_proxied(headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
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
/// takes effect.
///
/// Sub-accounts and the bootstrap `admin` placeholder are excluded. The
/// placeholder (email `admin@localhost`) is created by `ensure_admin_user`
/// on a bare admin-token `/me` call; it is NOT a solo owner, so matching it
/// here would mint an admin session instead of the documented first-run 404.
/// `is_login_ready_email` rejects it (and any empty/placeholder email),
/// leaving only genuine solo profiles created via `profile/local/create`.
fn resolve_solo_user(state: &AppState) -> Option<User> {
    let store = state.user_store.as_ref()?;
    let mut users: Vec<User> = store.list().ok()?;
    users.retain(|u| is_top_level_profile_id(state, &u.id) && is_login_ready_email(&u.email));
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
    headers: HeaderMap,
    Json(params): Json<ProfileLocalCreateParams>,
) -> Result<Json<SoloCreateResponse>, StatusCode> {
    solo_login_allowed(&state, Some(addr.ip()), &headers)?;
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
    headers: HeaderMap,
) -> Result<Json<SoloLoginResponse>, StatusCode> {
    solo_login_allowed(&state, Some(addr.ip()), &headers)?;
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

    fn solo_state_full(dir: &Path, mode: DeploymentMode, opt_in: bool) -> Arc<AppState> {
        let user_store = Arc::new(UserStore::open(dir).unwrap());
        let auth_manager = Arc::new(AuthManager::new(None, user_store.clone()));
        Arc::new(AppState {
            profile_store: Some(Arc::new(ProfileStore::open(dir).unwrap())),
            user_store: Some(user_store),
            auth_manager: Some(auth_manager),
            deployment_mode: mode,
            solo_login_enabled: opt_in,
            ..AppState::empty_for_tests()
        })
    }

    fn solo_state(dir: &Path) -> Arc<AppState> {
        solo_state_full(dir, DeploymentMode::Local, true)
    }

    fn solo_state_in_mode(dir: &Path, mode: DeploymentMode) -> Arc<AppState> {
        solo_state_full(dir, mode, true)
    }

    fn loopback() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000)))
    }

    fn remote() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([8, 8, 8, 8], 40000)))
    }

    fn no_headers() -> HeaderMap {
        HeaderMap::new()
    }

    fn proxied_headers() -> HeaderMap {
        // What Caddy (or any reverse proxy) adds in front of the daemon.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        h
    }

    fn params(name: &str, username: &str, email: &str) -> ProfileLocalCreateParams {
        ProfileLocalCreateParams {
            requested_id: None,
            name: name.into(),
            username: username.into(),
            email: email.into(),
            make_default: None,
        }
    }

    #[tokio::test]
    async fn solo_create_returns_profile_and_token_when_local_and_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let Json(resp) = solo_create(
            State(state.clone()),
            loopback(),
            no_headers(),
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
            no_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();

        let Json(resp) = solo_login(State(state.clone()), loopback(), no_headers())
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

        let err = solo_login(State(state), loopback(), no_headers())
            .await
            .expect_err("no solo profile yet → 404 so the SPA shows the create form");
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn solo_login_404_ignores_admin_placeholder() {
        // A bare admin-token `/me` call can create the disabled `admin`
        // placeholder (email admin@localhost) as a top-level user. That is
        // NOT a solo owner — solo_login must still 404 so the SPA shows the
        // create form rather than minting an admin session.
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());
        state
            .user_store
            .as_ref()
            .unwrap()
            .save(&User {
                id: "admin".into(),
                email: "admin@localhost".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();

        let err = solo_login(State(state), loopback(), no_headers())
            .await
            .expect_err("the admin placeholder is not a solo owner");
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn solo_create_403_when_opt_in_disabled() {
        // SECURITY: without the explicit operator opt-in, the solo endpoints
        // must be refused even on a Local loopback host. This is the guard
        // that keeps the Caddy-fronted fleet (which runs Local mode) safe.
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state_full(dir.path(), DeploymentMode::Local, false);

        let err = solo_create(
            State(state),
            loopback(),
            no_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .expect_err("solo must be refused unless explicitly opted in");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_create_403_when_proxied() {
        // SECURITY: a reverse proxy reaches the daemon over loopback but sets
        // forwarding headers. Such a request must be refused even with the
        // opt-in on, so the loopback check cannot be laundered through Caddy.
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());

        let err = solo_create(
            State(state),
            loopback(),
            proxied_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .expect_err("a proxied (X-Forwarded-For) request must be refused");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_login_403_when_proxied() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state(dir.path());
        let _ = solo_create(
            State(state.clone()),
            loopback(),
            no_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();

        let err = solo_login(State(state), loopback(), proxied_headers())
            .await
            .expect_err("a proxied login must be refused even with a valid profile");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn solo_create_403_when_not_local_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = solo_state_in_mode(dir.path(), DeploymentMode::Tenant);

        let err = solo_create(
            State(state),
            loopback(),
            no_headers(),
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
            no_headers(),
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
            no_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();

        let err = solo_login(State(state), remote(), no_headers())
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
            no_headers(),
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
            no_headers(),
            Json(params("Ada", "ada", "ada@example.com")),
        )
        .await
        .unwrap();
        assert!(first.result.created);

        let Json(second) = solo_create(
            State(state.clone()),
            loopback(),
            no_headers(),
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
