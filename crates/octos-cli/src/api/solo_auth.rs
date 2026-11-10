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
use super::ui_protocol_transport::{
    create_or_get_local_solo_profile, supports_local_solo_profile_create,
};
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

/// `pub(crate)` so the pairing endpoints (WEB-PAIRING-CONTRACT-5100)
/// reuse the SAME anti-laundering rule rather than restating it.
pub(crate) fn is_proxied(headers: &HeaderMap) -> bool {
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
pub(crate) fn resolve_solo_user(state: &AppState) -> Option<User> {
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

/// Derive `(username, email)` from a display name for name-only onboarding.
///
/// The username becomes both the user id and the profile id, so it must
/// satisfy the same constraints as `normalize_local_username` (ASCII
/// alnum + separators, 1-64 chars, not a reserved channel name). The email
/// is a clearly-local placeholder on the `solo.local` domain: it passes
/// `validate_local_email`, is `is_login_ready_email`-compatible (it can
/// never equal the `admin@localhost` placeholder), and signals in the UI
/// that no real address was ever collected.
fn derive_solo_credentials(name: &str) -> (String, String) {
    let mut username = String::new();
    let mut last_was_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            username.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !username.is_empty() && !last_was_dash {
            username.push('-');
            last_was_dash = true;
        }
    }
    let mut username = username.trim_matches('-').to_owned();
    // ASCII-only at this point, so byte truncation is char-boundary safe;
    // re-trim a separator the cut may have exposed.
    username.truncate(64);
    let mut username = username.trim_end_matches('-').to_owned();
    if username.is_empty() {
        username = "user".to_owned();
    }
    if octos_core::is_reserved_channel_name(&username) {
        username = format!("{username}-user");
    }
    let email = format!("{username}@solo.local");
    (username, email)
}

/// `POST /api/auth/solo/create` — onboard a local profile AND log in.
///
/// Public route (no auth middleware): the whole point is that no credential
/// exists yet. Gated at request time by [`solo_login_allowed`].
///
/// `username` / `email` may be left blank: they are derived from `name`
/// (see [`derive_solo_credentials`]) so the SPA's first-run form can ask
/// for a display name and nothing else.
pub async fn solo_create(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(params): Json<ProfileLocalCreateParams>,
) -> Result<Json<SoloCreateResponse>, StatusCode> {
    solo_login_allowed(&state, Some(addr.ip()), &headers)?;
    let mut params = params;
    if params.username.trim().is_empty() || params.email.trim().is_empty() {
        let (derived_username, derived_email) = derive_solo_credentials(&params.name);
        if params.username.trim().is_empty() {
            params.username = derived_username;
        }
        if params.email.trim().is_empty() {
            params.email = derived_email;
        }
    }
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

    fn solo_state(dir: &Path, opt_in: bool) -> Arc<AppState> {
        let user_store = Arc::new(UserStore::open(dir).unwrap());
        Arc::new(AppState {
            profile_store: Some(Arc::new(ProfileStore::open_unified(dir).unwrap())),
            user_store: Some(user_store.clone()),
            auth_manager: Some(Arc::new(AuthManager::new(None, user_store))),
            deployment_mode: DeploymentMode::Local,
            solo_login_enabled: opt_in,
            ..AppState::empty_for_tests()
        })
    }

    fn loopback() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000)))
    }

    #[tokio::test]
    async fn solo_create_403_when_opt_in_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let error = solo_create(
            State(solo_state(dir.path(), false)),
            loopback(),
            HeaderMap::new(),
            Json(ProfileLocalCreateParams {
                requested_id: None,
                name: "Ada".into(),
                username: "ada".into(),
                email: "ada@example.com".into(),
                make_default: None,
            }),
        )
        .await
        .expect_err("loopback alone must not bypass explicit solo opt-in");
        assert_eq!(error, StatusCode::FORBIDDEN);
    }
}
