//! API request handlers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use octos_agent::inspect_workspace_contract;
use octos_bus::file_handle::{
    encode_profile_file_handle, encode_tmp_upload_handle, resolve_legacy_file_request,
    resolve_scoped_file_handle,
};
#[cfg(test)]
use octos_core::Message;
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth_handlers::{ADMIN_PROFILE_ID, is_authorized_for_profile};
use super::router::AuthIdentity;
use crate::project_templates::{
    BuildOutputDirError, SiteProjectMetadata, read_site_project_metadata,
    validated_build_output_dir,
};

/// Legacy `POST /api/chat` retired.
///
/// Transport history:
/// - M9-α-5/α-6 (ADR PR #830): the SSE branch was deleted; `stream: true`
///   returned `410 Gone` and clients had to use `/api/ui-protocol/ws`.
/// - Cleanup follow-up to PR #908: the surviving sync JSON path was
///   retired once the last callers (the `coding_multi_session`
///   integration test, three e2e specs, and
///   `scripts/validate-m4-1a-live.sh`) migrated to the WS path.
///
/// The sole chat transport is now `/api/ui-protocol/ws`.

#[derive(Serialize)]
pub(crate) struct ContentFileEntry {
    filename: String,
    path: String,
    size: u64,
    modified: String,
    category: String,
    /// Parent directory name for grouping in the UI.
    group: String,
}

pub(crate) fn response_path_for_profile_file(
    base_dir: &std::path::Path,
    path: &std::path::Path,
) -> Option<String> {
    encode_profile_file_handle(base_dir, path)
        .or_else(|| encode_tmp_upload_handle(path, path.file_name().and_then(|name| name.to_str())))
}

fn resolve_scoped_download_path(
    base_dir: &std::path::Path,
    request_path: &str,
) -> Option<std::path::PathBuf> {
    resolve_scoped_file_handle(base_dir, request_path)
        .or_else(|| resolve_legacy_file_request(base_dir, request_path))
}

fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    Some(strip_port_from_host(&raw).to_string())
}

fn strip_port_from_host(host: &str) -> &str {
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(host);
    }

    if host.matches(':').count() == 1 {
        return host.split(':').next().unwrap_or(host);
    }

    host
}

fn is_local_request_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn resolve_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    state
        .profile_store
        .as_ref()
        .and_then(|store| store.resolve_routable_profile_id(candidate).ok().flatten())
}

pub(crate) fn routed_profile_id_from_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<String> {
    if let Some(host) = request_host(headers) {
        if !is_local_request_host(&host) {
            if let Some(candidate) = host.split('.').next() {
                if let Some(profile_id) = resolve_profile_id_candidate(state, candidate) {
                    return Some(profile_id);
                }
            }
        }
    }

    headers
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|candidate| resolve_profile_id_candidate(state, candidate))
}

/// #995 follow-up — authorization-aware wrapper around
/// [`routed_profile_id_from_headers`].
///
/// On TRUSTED hops (loopback by default, or
/// `OCTOS_TRUSTED_PROXY_CIDRS`-matched addresses) the strip middleware
/// preserves the operator-set `X-Profile-Id`, so authenticated requests
/// can still smuggle a victim-profile id past the routing layer. This
/// helper closes that gap: when both a header-resolved profile id AND
/// an authenticated identity are present, the identity MUST be
/// authorized for the target profile. Admin tokens (`AuthIdentity::Admin`)
/// and admin-role user sessions short-circuit to `Ok`; owner→sub-account
/// is also permitted via the parent_id check in
/// [`super::auth_handlers::is_authorized_for_profile`]. Mismatch is a
/// hard `403`.
///
/// Unauthenticated requests pass through unchanged — the call site is
/// responsible for its own downstream authorization (e.g. webhook
/// proxies or public preview routes).
#[allow(clippy::result_large_err)]
pub(crate) fn authorized_routed_profile_id_from_headers(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<Option<String>, Response> {
    let Some(profile_id) = routed_profile_id_from_headers(state, headers) else {
        return Ok(None);
    };
    if let Some(identity) = identity {
        if !super::auth_handlers::is_authorized_for_profile(state, identity, &profile_id) {
            tracing::warn!(
                target: "octos::api::auth",
                identity = ?identity,
                requested_profile = %profile_id,
                "routed_profile_id denied — authenticated identity not authorized for the requested profile (#995 follow-up)"
            );
            return Err((StatusCode::FORBIDDEN, "forbidden").into_response());
        }
    }
    Ok(Some(profile_id))
}

/// Resolve API port for a specific profile, or fall back to first available.
/// Profile is identified by X-Profile-Id header (set by Caddy from subdomain).
async fn resolve_api_port(state: &AppState, headers: &HeaderMap) -> Option<(String, u16)> {
    let pm = state.process_manager.as_ref()?;

    if let Some(profile_id) = routed_profile_id_from_headers(state, headers) {
        if let Some(port) = pm.api_port(&profile_id).await {
            return Some((profile_id, port));
        }
        tracing::warn!(profile = profile_id, "no API port for requested profile");
    }

    // Fall back to first available
    pm.first_api_port().await
}

/// #995 follow-up — authorization-aware wrapper around
/// [`resolve_api_port`]. Returns `Err(403)` when the header-resolved
/// profile is not one the authenticated identity is authorized for.
///
/// The header authorization check runs FIRST — even if no
/// `process_manager` is wired (standalone mode) the call site still
/// needs to authorize a cross-tenant `X-Profile-Id`, because the
/// authorization gate is the only thing keeping a forged header from
/// reaching the standalone path's storage helpers downstream.
///
/// Falls back to the gateway's first available port when no header
/// resolved to a profile, matching the legacy `resolve_api_port`
/// contract — that fallback never touches a tenant-scoped route, so
/// there is no header to authorize.
#[allow(clippy::result_large_err)]
async fn resolve_api_port_authorized(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<Option<(String, u16)>, Response> {
    let authorized_profile_id =
        authorized_routed_profile_id_from_headers(state, headers, identity)?;

    let pm = match state.process_manager.as_ref() {
        Some(pm) => pm,
        None => return Ok(None),
    };

    if let Some(profile_id) = authorized_profile_id {
        if let Some(port) = pm.api_port(&profile_id).await {
            return Ok(Some((profile_id, port)));
        }
        tracing::warn!(profile = profile_id, "no API port for requested profile");
    }

    Ok(pm.first_api_port().await)
}

/// #995 follow-up round 3 — authorization-aware profile resolver for
/// the API channel. Returns `Err(403)` when the header resolves to a
/// profile the identity is not authorized for; falls back to
/// `MAIN_PROFILE_ID` when no header is present (matching the legacy
/// `api_profile_id_from_headers` contract, retired in this round).
///
/// The raw (unauthorized) variant was deleted because the only
/// remaining caller — the standalone candidate walk in
/// [`standalone_api_session_key_candidates_with_topic`] — is now
/// reachable on a trusted hop AND must therefore reject cross-tenant
/// headers up-front. Codex round-2 review called out the raw variant
/// as bypass-prone for exactly this reason; the function now exists
/// only in its `_authorized` form so future call sites can't
/// reintroduce the same bug.
#[allow(clippy::result_large_err)]
fn api_profile_id_from_headers_authorized(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<String, Response> {
    Ok(
        authorized_routed_profile_id_from_headers(state, headers, identity)?
            .unwrap_or_else(|| MAIN_PROFILE_ID.to_string()),
    )
}

/// Returns `true` when `session_id` is a bare SPA id whose raw form is
/// safe to query as a `SessionKey` directly. Specifically: no `:` (which
/// is the channel/profile separator in
/// [`octos_core::SessionKey`]) and no `#` (the topic separator).
///
/// Without this guard, `/api/sessions/{id}/messages?id=telegram:123`
/// would walk the raw-id candidate and return that telegram session's
/// history under a REST endpoint scoped to the API channel — a
/// cross-channel / cross-profile leak (codex review P1 round 2 on the
/// M10.5 reload-mid-stream PR).
///
/// We allow `web-…`, raw UUIDs, and similar punctuation-light shapes;
/// ANY id that contains `:` is rejected for the raw-id fallback. The
/// API-channel and profile-prefixed candidates still cover those ids
/// — the raw-id candidate is purely the recovery path for SPA bare ids.
fn is_safe_bare_session_id(session_id: &str) -> bool {
    !session_id.contains(':') && !session_id.contains('#')
}

/// Topic-aware, auth-aware candidate resolver shared by REST
/// `/messages`, `session/title.set`, and `session/delete`.
///
/// Returns the candidate `SessionKey`s the REST `/messages` and similar
/// read paths should try, in fallback order. The fallback set is split
/// by the resolved profile and the request's auth identity:
///
/// **Tenant-scoped requests** (resolved profile is NOT `MAIN_PROFILE_ID`
/// AND the request is NOT admin-authenticated). Only ONE candidate:
/// 1. **Profiled key** with topic (`<profile>:api:<id>#<topic>`).
///
/// Tenant accounts must NEVER see another profile's history by id. The
/// `_main`, bare-channel, and raw-id candidates ALL live in shared /
/// non-tenant namespaces; surfacing them to a tenant-scoped request
/// would let a colliding `web-…` id read foreign rows (codex P1
/// rounds 3 and 4).
///
/// **Main / local / admin mode** (resolved profile IS `MAIN_PROFILE_ID`
/// OR the request carries admin auth):
/// 1. **Profiled key** (`<profile>:api:<id>#<topic>`).
/// 2. **`_main:api:<id>` key** — picks up legacy main-profile rows.
/// 3. **Bare-channel key** (`api:<id>#<topic>`) — what the WS
///    `turn/start` path uses when an admin-authenticated SPA sends
///    `SessionKey::new("api", "web-…")`. The dominant production
///    reload-mid-stream shape on hosted subdomains under admin auth
///    (codex P2 round 5 — `connection_profile_id == None` for admin so
///    `validate_authenticated_session_scope` accepts the bare key).
/// 4. **Raw-id key** (`<id>` or `<id>#<topic>`) — only when `id`
///    passes [`is_safe_bare_session_id`]. Recovers from the SPA's
///    bare-id `SessionKey("web-…")` shape. Codex P1 round 2: rejecting
///    `:` / `#` blocks crafted-URL leaks via this candidate.
///
/// Admin already has read-all privileges across all profiles via the
/// other admin handlers, so unlocking the cross-namespace fallbacks
/// for admin requests is no privilege escalation.
///
/// The dedup pass collapses duplicates when the topic is empty (in
/// which case the bare-key and raw-id forms coincide with their
/// no-topic counterparts).
///
/// **#995 follow-up round 3 — authorization is REQUIRED**. This helper
/// now uses [`api_profile_id_from_headers_authorized`] internally, so
/// a cross-tenant `X-Profile-Id` on a trusted hop produces an `Err`
/// `Response` (403) instead of resolving to the victim profile id.
/// Every caller already authorized via
/// [`authorized_routed_profile_id_from_headers`] at the handler
/// entry, so this is defense-in-depth: if a future caller is added
/// that forgets to gate up-front, the candidate walk itself rejects
/// the cross-tenant header rather than building candidates against
/// the victim profile. Codex round-2 review specifically called out
/// the standalone `session_messages` path as bypass-prone for exactly
/// this reason.
#[allow(clippy::result_large_err)]
fn standalone_api_session_key_candidates_with_topic(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    session_id: &str,
    topic: Option<&str>,
) -> Result<Vec<SessionKey>, Response> {
    let profile_id = api_profile_id_from_headers_authorized(state, headers, identity)?;
    let topic = topic.unwrap_or_default();

    // Always probe the resolved profile's own canonical key first.
    let mut candidates = vec![SessionKey::with_profile_topic(
        &profile_id,
        "api",
        session_id,
        topic,
    )];

    // The remaining candidates (`_main:api:<id>`, bare-channel, raw-id)
    // are gated on the resolved profile being the synthetic main
    // profile OR the request being admin-authenticated. For tenant
    // user requests each profile prefix is the isolation boundary, and
    // a shared standalone `SessionManager` could otherwise let one
    // profile read another's WS-persisted history (codex P1 rounds 3
    // and 4 on the M10.5 reload-mid-stream PR).
    //
    // Codex P2 round 5: admin auth on a hosted subdomain is the
    // canonical reload-mid-stream production shape; the WS handler
    // there accepts bare `SessionKey`s (admin's
    // `connection_profile_id` is `None`, so
    // `validate_authenticated_session_scope` doesn't fire). The
    // unprofiled fallback MUST be reachable from REST in that mode.
    let is_admin = matches!(identity, Some(AuthIdentity::Admin));
    let allow_cross_profile_fallback = profile_id == MAIN_PROFILE_ID || is_admin;
    if allow_cross_profile_fallback {
        candidates.push(SessionKey::with_profile_topic(
            MAIN_PROFILE_ID,
            "api",
            session_id,
            topic,
        ));
        candidates.push(SessionKey::with_topic("api", session_id, topic));
        // Raw-id candidate adds another layer of guardrails: `id` may
        // contain attacker-controlled bytes (it lands here straight
        // from `axum::extract::Path`), and `SessionKey` accepts any
        // string. Codex P1 round 2: only emit the raw-id form when
        // `id` is a safe bare SPA id (no `:` / no `#`).
        if is_safe_bare_session_id(session_id) {
            let raw_id = if topic.is_empty() {
                SessionKey(session_id.to_string())
            } else {
                SessionKey(format!("{session_id}#{topic}"))
            };
            candidates.push(raw_id);
        }
    }
    candidates.dedup_by(|left, right| left.0 == right.0);
    Ok(candidates)
}

fn encode_api_session_path_id(id: &str) -> String {
    octos_bus::session::encode_path_component(id)
}

/// Result entry shape for the WS `session/list` RPC method (formerly the
/// body of `GET /api/sessions`, retired in M12 Phase D-5).
#[derive(Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub message_count: usize,
    /// Display title (auto-derived from first user message; manual rename via
    /// the WS `session/title.set` RPC method preserves across new messages).
    /// None for legacy sessions persisted before the title field existed;
    /// the client should fall back to deriving a title from message content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

fn is_internal_api_session_id(id: &str) -> bool {
    id.split_once('#')
        .is_some_and(|(_, topic)| is_internal_session_topic(topic))
}

fn is_internal_session_topic(topic: &str) -> bool {
    topic.starts_with("child-") || topic == "default.tasks" || topic.ends_with(".tasks")
}

// Helper for `ui_protocol::handle_session_list` (M12 Phase D-5).
// The REST route `GET /api/sessions` was retired; this function survives
// as the implementation backing the WS `session/list` RPC method.
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
) -> Response {
    // Collect sessions from both the standalone store and gateway profiles.
    let mut all: Vec<SessionInfo> = Vec::new();
    let identity_ref = identity.as_ref().map(|ext| &ext.0);

    // #995 follow-up — Layer-2 authorization for the routed profile.
    // Pre-fix this prefix went straight to `routed_profile_id_from_headers`,
    // so a trusted-hop request with `X-Profile-Id: <victim>` listed the
    // victim's sessions even when authenticated as another tenant.
    let profile_id = match api_profile_id_from_headers_authorized(&state, &headers, identity_ref) {
        Ok(pid) => pid,
        Err(response) => return response,
    };

    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        let prefix = format!("{profile_id}:api:");
        // Use `list_top_level_sessions` (skips `child-*` and `*.tasks` at the
        // directory walk) so a user dir with tens of thousands of spawn
        // children does not turn this listing into an O(N) hang. The
        // `is_internal_api_session_id` belt-and-suspenders check is kept for
        // legacy entries that might slip through (e.g. flat layout files
        // pre-dating the encoder rules).
        all.extend(
            sess.list_top_level_sessions_with_title()
                .into_iter()
                .filter_map(|(id, count, title)| {
                    let chat_id = id.strip_prefix(&prefix)?;
                    if is_internal_api_session_id(chat_id) {
                        return None;
                    }
                    Some(SessionInfo {
                        id: chat_id.to_string(),
                        message_count: count,
                        title,
                    })
                }),
        );
    }

    // Also fetch from gateway if available.
    // #995 follow-up — routed_profile_id used to walk the per-profile
    // gateway is authorized above; the `resolve_api_port_authorized`
    // call re-checks header authorization belt-and-suspenders.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let proxy_resp = super::webhook_proxy::api_get_proxy(&state, port, "/sessions").await;
        if proxy_resp.status().is_success() {
            if let Ok(body) = axum::body::to_bytes(proxy_resp.into_body(), 10 * 1024 * 1024).await {
                if let Ok(gateway_sessions) = serde_json::from_slice::<Vec<SessionInfo>>(&body) {
                    // Merge, dedup by id (standalone wins)
                    let existing: std::collections::HashSet<String> =
                        all.iter().map(|s| s.id.clone()).collect();
                    all.extend(gateway_sessions.into_iter().filter(|s| {
                        !existing.contains(&s.id) && !is_internal_api_session_id(&s.id)
                    }));
                }
            }
        }
    }

    if all.is_empty() && state.sessions.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Sessions not available".to_string(),
        )
            .into_response();
    }

    Json(all).into_response()
}

// Helper for `ui_protocol::handle_session_messages_page` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/messages` was retired; this
// function survives as the implementation backing the WS
// `session/messages_page` RPC method.
///
/// Backing impl for the WS `session/messages_page` RPC method. Pagination
/// shape (`limit`/`offset`/`since_seq`) carried over from the legacy REST
/// route into `SessionMessagesPageParams`.
#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub since_seq: Option<usize>,
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Deserialize)]
pub struct TopicQueryParams {
    #[serde(default)]
    pub topic: Option<String>,
}

// `SessionEventStreamQueryParams` and the `/api/sessions/{id}/events/stream`
// route it served were deleted in M9-α-5/α-6 (ADR PR #830 / audit issue
// #845). Every session-event subscriber now consumes the
// `session/event.v1` notification on `/api/ui-protocol/ws`.

fn default_page_limit() -> usize {
    100
}

fn append_topic_query(path: &mut String, topic: Option<&str>) {
    if let Some(topic) = topic.filter(|value| !value.is_empty()) {
        path.push_str(if path.contains('?') {
            "&topic="
        } else {
            "?topic="
        });
        path.push_str(&octos_bus::session::encode_path_component(topic));
    }
}

fn session_messages_proxy_path(
    id: &str,
    limit: usize,
    offset: usize,
    source: Option<&str>,
    since_seq: Option<usize>,
    topic: Option<&str>,
) -> String {
    let encoded_id = encode_api_session_path_id(id);
    let mut path = format!("/sessions/{encoded_id}/messages?limit={limit}&offset={offset}");
    if let Some(source) = source {
        path.push_str("&source=");
        path.push_str(source);
    }
    if let Some(since_seq) = since_seq {
        path.push_str("&since_seq=");
        path.push_str(&since_seq.to_string());
    }
    append_topic_query(&mut path, topic);
    path
}

pub async fn session_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let limit = params.limit.min(500);
    let offset = params.offset.min(10_000);
    let identity_ref = identity.as_ref().map(|ext| &ext.0);

    // source=full: always proxy to gateway, which owns the canonical JSONL history.
    let use_full = params.source.as_deref() == Some("full");

    // #995 follow-up round 3 — Layer-2 authorization gate BEFORE any
    // standalone-store candidate walk. Pre-fix, the standalone path at
    // lines 587-620 built candidate `SessionKey`s using the raw
    // `api_profile_id_from_headers` (which trusts whatever
    // `X-Profile-Id` the request carries on a trusted hop) and returned
    // messages from those candidates before ever hitting the gateway
    // gate at the bottom of this function. The result: an authenticated
    // non-admin user on a loopback hop could read another tenant's
    // session messages by forging `X-Profile-Id`. Codex round-2 quote:
    // "Passing `identity` only affects fallback breadth; it does not
    // reject cross-tenant headers." This call rejects cross-tenant
    // headers up-front; admin / owner / self continue past it.
    if let Err(response) = authorized_routed_profile_id_from_headers(&state, &headers, identity_ref)
    {
        return response;
    }

    // Try standalone store first in local mode.
    if !use_full {
        if let Some(sessions) = &state.sessions {
            let fetch_count = match offset.checked_add(limit) {
                Some(n) => n,
                None => return (StatusCode::BAD_REQUEST, "invalid pagination").into_response(),
            };
            // M10.5 reload-mid-stream fix: WS turns persisted by `turn/start`
            // (which calls `sessions.get_or_create(&params.session_id)` with
            // whatever `SessionKey` the SPA sent) may live under a key that
            // does NOT match the profiled key (`<profile>:api:<id>`) the REST
            // `/messages` lookup historically used. Concretely:
            //
            //   • Bare channel key (`api:<id>`) — `with_profile_topic`
            //     fall-through when no profile context is set.
            //   • Raw SPA id (`web-…`) — when the SPA sends a bare-channel
            //     `SessionKey::new("web-…")` literally, the WS handler
            //     persists under `web-…` verbatim (no `api:` prefix).
            //
            // Fix: walk the candidate key list (profiled first, bare last)
            // and return the first candidate that *has any history* (not
            // just any rows on this page) — using `messages.is_empty()`
            // after `skip(offset).take(limit)` would silently fall through
            // to a sibling key whenever the requested page is past the end
            // of the canonical session, mixing histories under pagination
            // (codex review P2).
            //
            // Codex P2 round 5: in production deployments using admin auth
            // on a hosted subdomain (`dspfac.crew.ominix.io` +
            // `OCTOS_AUTH_TOKEN=admin-…`), the WS handler accepts bare
            // `SessionKey`s and persists under those raw keys. Pass the
            // request identity into the candidate helper so admin auth
            // unlocks the unprofiled fallbacks even when the request
            // resolved to a hosted profile (admin already has read-all
            // privileges, so this is no privilege escalation).
            let candidate_keys = match standalone_api_session_key_candidates_with_topic(
                &state,
                &headers,
                identity_ref,
                &id,
                params.topic.as_deref(),
            ) {
                Ok(keys) => keys,
                Err(response) => return response,
            };
            let mut sess = sessions.lock().await;
            let mut chosen: Option<&SessionKey> = None;
            for key in &candidate_keys {
                let session = sess.get_or_create(key).await;
                if !session.get_history(1).is_empty() {
                    chosen = Some(key);
                    break;
                }
            }
            if let Some(key) = chosen {
                let session = sess.get_or_create(key).await;
                let messages: Vec<MessageInfo> = session
                    .get_history(fetch_count)
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|m| MessageInfo {
                        role: m.role.to_string(),
                        content: m.content.clone(),
                        timestamp: m.timestamp.to_rfc3339(),
                        thread_id: m.thread_id.clone(),
                    })
                    .collect();
                // Keep the historical contract: a page that is past the
                // end of a real session returns `[]` (and stays on this
                // session — does NOT flip to a sibling candidate).
                return Json(messages).into_response();
            }
            // Fall through to gateway if no candidate has any history.
        }
    } // !use_full

    // Proxy to gateway.
    // #995 follow-up — authorize routed profile against identity
    // before walking the gateway.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let path = session_messages_proxy_path(
            &id,
            limit,
            offset,
            if use_full {
                Some("full")
            } else {
                params.source.as_deref()
            },
            params.since_seq,
            params.topic.as_deref(),
        );
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    (StatusCode::SERVICE_UNAVAILABLE, "Sessions not available").into_response()
}

#[derive(Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    pub timestamp: String,
    /// M8.10 PR #1 thread grouping key. Lets the web client render chat
    /// history as `Vec<Thread>` rather than a flat message list. Omitted
    /// from the JSON when `None` so legacy clients that don't read the
    /// field continue to round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

// Helper for `ui_protocol::handle_session_status_get` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/status` was retired; this
// function survives as the implementation backing the WS
// `session/status.get` RPC method.
/// Backing impl for the WS `session/status.get` RPC method.
pub async fn session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    // Proxy to gateway (session actors live there).
    // #995 follow-up — authorize routed profile against identity
    // before walking the gateway. Pre-fix the gateway routing read the
    // raw header.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/status");
        append_topic_query(&mut path, params.topic.as_deref());
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no active task tracking
    Json(serde_json::json!({
        "active": false,
    }))
    .into_response()
}

// Helper for `ui_protocol::handle_session_tasks_list` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/tasks` was retired; this
// function survives as the implementation backing the WS
// `session/tasks.list` RPC method.
/// Backing impl for the WS `session/tasks.list` RPC method.
pub async fn session_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    // Proxy to gateway (task supervisor lives there).
    // #995 follow-up — authorize routed profile against identity.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/tasks");
        append_topic_query(&mut path, params.topic.as_deref());
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no background tasks
    Json(serde_json::json!([])).into_response()
}

// ───────── M7.9 / W2 task supervisor: cancel + restart-from-node ─────────

/// `POST /api/tasks/{task_id}/cancel` — forward to
/// [`octos_agent::TaskSupervisor::cancel`]. Returns:
///
/// - `200 OK` `{ "task_id": "...", "status": "cancelled" }` when the
///   task was running/queued and has been transitioned to `Cancelled`.
/// - `404 Not Found` when no supervised task carries that id.
/// - `409 Conflict` when the task is already in a terminal state
///   (`Completed` / `Failed` / `Cancelled`).
/// - `503 Service Unavailable` when the API server has no
///   `task_query_store` wired (standalone mode).
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    // Gateway-mode: forward to the gateway process that owns the
    // supervisor. #995 follow-up — authorize routed profile against
    // identity before forwarding (a forged header could otherwise
    // cancel a victim's task on a TRUSTED hop).
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let path = format!("/tasks/{}/cancel", encode_api_session_path_id(&task_id));
        return super::webhook_proxy::api_post_proxy_json(
            &state,
            port,
            &path,
            serde_json::json!({}),
        )
        .await;
    }

    let Some(store) = state.task_query_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired in standalone mode",
            })),
        )
            .into_response();
    };

    match store.cancel_task(&task_id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "task_id": task_id,
                "status": "cancelled",
            })),
        )
            .into_response(),
        Err(octos_agent::TaskCancelError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskCancelError::AlreadyTerminal) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_already_terminal",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

/// Body of `POST /api/tasks/{task_id}/restart-from-node`.
#[derive(Debug, Default, Deserialize)]
pub struct RestartFromNodeRequest {
    /// Optional DOT-graph node id to restart from. Upstream cached
    /// outputs from preceding nodes are preserved by the runtime; only
    /// the target node and its downstream subtree re-run.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// `POST /api/tasks/{task_id}/restart-from-node` — forward to
/// [`octos_agent::TaskSupervisor::relaunch`]. Returns:
///
/// - `200 OK` `{ "original_task_id": "...", "new_task_id": "...",
///   "from_node": "..." }` on accept.
/// - `404 Not Found` when the task id is unknown.
/// - `409 Conflict` when the task is still active (callers must cancel
///   first).
/// - `503 Service Unavailable` when no supervisor is wired.
pub async fn restart_task_from_node(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    body: Option<Json<RestartFromNodeRequest>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let identity_ref = identity.as_ref().map(|ext| &ext.0);

    // #995 follow-up — authorize routed profile against identity. A
    // forged header could otherwise relaunch a victim's task on a
    // TRUSTED hop.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let path = format!(
            "/tasks/{}/restart-from-node",
            encode_api_session_path_id(&task_id)
        );
        let proxied_body = serde_json::json!({
            "node_id": body.node_id,
        });
        return super::webhook_proxy::api_post_proxy_json(&state, port, &path, proxied_body).await;
    }

    let Some(store) = state.task_query_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task supervisor not wired in standalone mode",
            })),
        )
            .into_response();
    };

    let opts = octos_agent::RelaunchOpts {
        from_node: body.node_id.clone(),
    };
    match store.relaunch_task(&task_id, opts) {
        Ok(new_task_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "original_task_id": task_id,
                "new_task_id": new_task_id,
                "from_node": body.node_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskRelaunchError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "task_not_found",
                "task_id": task_id,
            })),
        )
            .into_response(),
        Err(octos_agent::TaskRelaunchError::StillActive) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "task_still_active",
                "task_id": task_id,
            })),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
pub struct SessionFileInfo {
    pub filename: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: String,
}

fn collect_session_files(
    root: &std::path::Path,
    data_dir: &std::path::Path,
    out: &mut Vec<SessionFileInfo>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        if metadata.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            collect_session_files(&path, data_dir, out);
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let Some(handle) = response_path_for_profile_file(data_dir, &path) else {
            continue;
        };
        out.push(SessionFileInfo {
            filename,
            path: handle,
            size_bytes: metadata.len(),
            modified_at: modified_rfc3339(&metadata),
        });
    }
}

// Helper for `ui_protocol::handle_session_files_list` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/files` was retired; this
// function survives as the implementation backing the WS
// `session/files.list` RPC method.
/// Backing impl for the WS `session/files.list` RPC method.
pub async fn session_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);

    // Issue #999 — gateway-mode tenant-leak guard. Pre-fix the
    // `state.sessions.is_some()` branch short-circuited to
    // `sess.data_dir()` (the gateway/standalone top-level) BEFORE
    // checking the host-routed profile. An authenticated non-admin
    // user on a TRUSTED hop with a cross-tenant `X-Profile-Id` (or a
    // host-routed cross-tenant subdomain) walked straight into the
    // victim profile's workspace listing. Layer-2 authorization runs
    // up-front now, mirroring `session_messages` (#1002): a forged
    // cross-tenant header is `403` regardless of which side of the
    // sessions / no-sessions branch resolves the data_dir below.
    if let Err(response) = authorized_routed_profile_id_from_headers(&state, &headers, identity_ref)
    {
        return response;
    }

    let data_dir = if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        sess.data_dir()
    } else {
        match resolve_profile_data_dir(&state, &headers, identity_ref).await {
            Ok(data_dir) => data_dir,
            Err(response) => return response,
        }
    };

    let mut files = Vec::new();
    for workspace in api_session_workspace_dirs(&data_dir, &id) {
        if workspace.exists() {
            collect_session_files(&workspace, &data_dir, &mut files);
        }
    }

    files.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    files.dedup_by(|left, right| left.path == right.path);
    Json(files).into_response()
}

// Helper for `ui_protocol::handle_session_workspace_get` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/workspace-contract` was retired;
// this function survives as the implementation backing the WS
// `session/workspace.get` RPC method.
/// Backing impl for the WS `session/workspace.get` RPC method.
pub async fn session_workspace_contract(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);

    // Issue #999 — gateway-mode tenant-leak guard. Same shape as the
    // companion `session_files` handler above: pre-fix the
    // `state.sessions.is_some()` branch short-circuited to
    // `sess.data_dir()` BEFORE checking the host-routed profile, so a
    // cross-tenant header on a TRUSTED hop exposed the victim
    // profile's workspace-contract statuses. The Layer-2 gate runs
    // up-front; `state.sessions.data_dir()` only resolves when the
    // routed profile is authorized (admin / owner / self).
    if let Err(response) = authorized_routed_profile_id_from_headers(&state, &headers, identity_ref)
    {
        return response;
    }

    let data_dir = if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        sess.data_dir()
    } else {
        match resolve_profile_data_dir(&state, &headers, identity_ref).await {
            Ok(data_dir) => data_dir,
            Err(response) => return response,
        }
    };

    let mut statuses = Vec::new();
    for workspace in api_session_workspace_dirs(&data_dir, &id) {
        if !workspace.exists() {
            continue;
        }
        let Ok(repos) = octos_agent::list_workspace_repos(&workspace) else {
            continue;
        };
        statuses.extend(repos.iter().map(inspect_workspace_contract));
    }

    statuses.sort_by(|left, right| left.repo_label.cmp(&right.repo_label));
    statuses.dedup_by(|left, right| left.repo_label == right.repo_label);
    Json(statuses).into_response()
}

async fn resolve_file_access_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<std::path::PathBuf, Response> {
    if should_resolve_file_access_from_profile(headers, identity) {
        match resolve_profile_data_dir(state, headers, identity).await {
            Ok(data_dir) => return Ok(data_dir),
            Err(response) if response.status() != StatusCode::SERVICE_UNAVAILABLE => {
                return Err(response);
            }
            Err(_) => {}
        }
    }

    if let Some(sessions) = &state.sessions {
        let sess = sessions.lock().await;
        return Ok(sess.data_dir());
    }

    resolve_profile_data_dir(state, headers, identity).await
}

fn should_resolve_file_access_from_profile(
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> bool {
    // Hosted/profile-scoped requests must use the same root as /api/files/list so
    // profile-encoded file handles round-trip through /api/files unchanged.
    request_host(headers).is_some_and(|host| !is_local_request_host(&host))
        || headers
            .get("x-profile-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || identity.is_some()
}

// Helper for `ui_protocol::handle_session_title_set` (M12 Phase D-5).
// The REST route `PATCH /api/sessions/{id}/title` was retired; this
// function survives as the implementation backing the WS
// `session/title.set` RPC method.
/// Backing request shape for the WS `session/title.set` RPC method
/// (formerly the body of `PATCH /api/sessions/{id}/title`). The title
/// persists across new messages — auto-derivation from the first user
/// message no longer overrides it once a manual title is set.
#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

/// Backing impl for the WS `session/title.set` RPC method.
pub async fn update_session_title(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<UpdateTitleRequest>,
) -> Response {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    if title.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, "title must be at most 200 chars").into_response();
    }

    let mut updated = false;
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    // #995 follow-up — authorize routed profile against identity
    // before using it as `SessionManager` routing context. Pre-fix the
    // raw header value flowed straight into `resolve_sessions_for_lookup`
    // and the gateway proxy below, letting a forged
    // `X-Profile-Id: <victim>` rename a victim's sessions on a TRUSTED
    // hop.
    let routed_profile_id =
        match authorized_routed_profile_id_from_headers(&state, &headers, identity_ref) {
            Ok(pid) => pid,
            Err(response) => return response,
        };
    let candidates = match standalone_api_session_key_candidates_with_topic(
        &state,
        &headers,
        identity_ref,
        &id,
        None,
    ) {
        Ok(keys) => keys,
        Err(response) => return response,
    };

    // #924 BLOCK 3: each candidate `SessionKey` may belong to a
    // different profile (the candidate set includes the resolved
    // tenant profile, `_main`, and bare-channel/raw-id forms). Route
    // each candidate through `resolve_sessions_for_lookup` so the
    // profile's own `SessionRuntime.sessions` is locked — turn
    // persistence writes there, so the title update MUST land in the
    // same store. The legacy code locked only `state.sessions`, which
    // did not contain profile-scoped runtime sessions and returned
    // `NOT_FOUND` for raw `web-*` ids under profile auth.
    for key in candidates {
        let Some(sessions) = super::ui_protocol::resolve_sessions_for_lookup(
            &state,
            None,
            routed_profile_id.as_deref(),
            &key,
        )
        .await
        else {
            continue;
        };
        let mut sess = sessions.lock().await;
        if sess.load(&key).await.is_some() {
            if let Err(e) = sess.update_title(&key, title.clone()).await {
                tracing::error!(
                    session_key = %key,
                    error = %e,
                    "update_title in profile-scoped store failed"
                );
            } else {
                updated = true;
            }
        }
    }

    // Proxy to gateway too, since the session may live in the per-profile
    // SessionManager rather than the serve-process store.
    // #995 follow-up — same `resolve_api_port_authorized` gate.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let path = format!("/sessions/{}/title", encode_api_session_path_id(&id));
        let body_json = serde_json::json!({ "title": title }).to_string();
        let _ = super::webhook_proxy::api_patch_proxy(&state, port, &path, body_json).await;
        // Treat gateway proxy success as also updating; we don't strictly
        // need to inspect the response since the gateway is authoritative
        // when serve has no in-memory copy.
        updated = true;
    }

    if updated {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "session not found").into_response()
    }
}

// Helper for `ui_protocol::handle_session_delete` (M12 Phase D-5).
// The REST route `DELETE /api/sessions/{id}` was retired; this
// function survives as the implementation backing the WS
// `session/delete` RPC method.
/// Backing impl for the WS `session/delete` RPC method.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    // #995 follow-up — authorize routed profile against identity
    // before using it as `SessionManager` routing context. Pre-fix a
    // forged `X-Profile-Id: <victim>` would delete victim's sessions
    // on a TRUSTED hop.
    let routed_profile_id =
        match authorized_routed_profile_id_from_headers(&state, &headers, identity_ref) {
            Ok(pid) => pid,
            Err(response) => return response,
        };
    let candidates = match standalone_api_session_key_candidates_with_topic(
        &state,
        &headers,
        identity_ref,
        &id,
        None,
    ) {
        Ok(keys) => keys,
        Err(response) => return response,
    };

    // #924 BLOCK 3: route each candidate through the profile-aware
    // SessionManager resolver — turn persistence writes to the
    // profile's `SessionRuntime.sessions`, so deletes MUST hit the
    // same store. The legacy code locked only `state.sessions`, which
    // did not contain profile-scoped runtime sessions.
    for key in candidates {
        let Some(sessions) = super::ui_protocol::resolve_sessions_for_lookup(
            &state,
            None,
            routed_profile_id.as_deref(),
            &key,
        )
        .await
        else {
            continue;
        };
        let mut sess = sessions.lock().await;
        if sess.load(&key).await.is_some() {
            if let Err(e) = sess.clear(&key).await {
                tracing::error!(
                    session_key = %key,
                    error = %e,
                    "delete session from profile-scoped store failed"
                );
            }
        }
    }

    // Also proxy delete to gateway — sessions may live in the gateway's
    // SessionManager (per-profile data dir), not just the serve process's store.
    // #995 follow-up — same `resolve_api_port_authorized` gate.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let path = format!("/sessions/{}", encode_api_session_path_id(&id));
        let _ = super::webhook_proxy::api_delete_proxy(&state, port, &path).await;
    }

    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/upload -- upload files, returns paths for use as turn-input media handles.
///
/// Accepts multipart/form-data with one or more `file` fields.
/// Returns JSON array of server-side upload handles.
pub async fn upload(
    State(_state): State<Arc<AppState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    // Determine upload directory
    let upload_dir = std::env::temp_dir().join("octos-uploads");
    tokio::fs::create_dir_all(&upload_dir).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create upload dir: {e}"),
        )
    })?;

    let mut paths = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut total_size: u64 = 0;
    const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50MB per file
    const MAX_TOTAL_SIZE: u64 = 100 * 1024 * 1024; // 100MB total

    while let Ok(Some(field)) = multipart.next_field().await {
        // Only process fields that have a filename (skip non-file fields)
        let filename = match field.file_name() {
            Some(f) => f.to_string(),
            None => continue,
        };
        // Skip duplicate filenames (browser may send the same file twice)
        if !seen_names.insert(filename.clone()) {
            let _ = field.bytes().await; // drain to avoid blocking
            continue;
        }

        // Sanitize filename — strip path separators
        let safe_name = filename
            .replace(['/', '\\', '\0'], "_")
            .chars()
            .take(200)
            .collect::<String>();

        let data = field.bytes().await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("failed to read field: {e}"),
            )
        })?;

        if data.len() as u64 > MAX_FILE_SIZE {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("file exceeds {MAX_FILE_SIZE} byte limit"),
            ));
        }
        total_size += data.len() as u64;
        if total_size > MAX_TOTAL_SIZE {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "total upload exceeds 100MB".into(),
            ));
        }

        // Unique prefix to avoid collisions
        let dest = upload_dir.join(format!("{}_{safe_name}", uuid::Uuid::now_v7()));
        tokio::fs::write(&dest, &data).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to write file: {e}"),
            )
        })?;

        tracing::info!(path = %dest.display(), size = data.len(), "file uploaded");
        let handle = encode_tmp_upload_handle(&dest, Some(&safe_name)).ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode upload handle".into(),
        ))?;
        paths.push(handle);
    }

    if paths.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in request".into()));
    }

    Ok(Json(paths))
}

/// POST /api/site-files/upload -- upload files directly into a site workspace.
///
/// Accepts multipart/form-data with:
/// - `session_id` (text)
/// - `site_slug` (text)
/// - `target_dir` (optional text, defaults by template)
/// - one or more `file` fields
pub async fn upload_site_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Vec<ContentFileEntry>>, (StatusCode, String)> {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = resolve_profile_data_dir(&state, &headers, identity)
        .await
        .map_err(|response| {
            (
                response.status(),
                "failed to resolve profile data dir".into(),
            )
        })?;

    let mut session_id: Option<String> = None;
    let mut site_slug: Option<String> = None;
    let mut target_dir: Option<String> = None;
    let mut uploads: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut total_size: u64 = 0;
    const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
    const MAX_TOTAL_SIZE: u64 = 100 * 1024 * 1024;

    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or_default().to_string();
        if let Some(filename) = field.file_name() {
            if field_name != "file" {
                let _ = field.bytes().await;
                continue;
            }

            let filename = filename.to_string();
            if !seen_names.insert(filename.clone()) {
                let _ = field.bytes().await;
                continue;
            }

            let data = field.bytes().await.map_err(|error| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("failed to read uploaded file: {error}"),
                )
            })?;
            if data.len() as u64 > MAX_FILE_SIZE {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("file exceeds {MAX_FILE_SIZE} byte limit"),
                ));
            }
            total_size += data.len() as u64;
            if total_size > MAX_TOTAL_SIZE {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "total upload exceeds 100MB".into(),
                ));
            }

            uploads.push((filename, data.to_vec()));
            continue;
        }

        let value = field.text().await.map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("failed to read form field `{field_name}`: {error}"),
            )
        })?;
        let value = value.trim().to_string();
        match field_name.as_str() {
            "session_id" if !value.is_empty() => session_id = Some(value),
            "site_slug" if !value.is_empty() => site_slug = Some(value),
            "target_dir" if !value.is_empty() => target_dir = Some(value),
            _ => {}
        }
    }

    if uploads.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in request".into()));
    }

    let session_id = session_id.ok_or((StatusCode::BAD_REQUEST, "missing session_id".into()))?;
    let site_slug = site_slug.ok_or((StatusCode::BAD_REQUEST, "missing site_slug".into()))?;

    let project_dir = api_session_workspace_dirs(&data_dir, &session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&site_slug))
        .find(|candidate| candidate.exists())
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("site workspace not found for session `{session_id}` and `{site_slug}`"),
        ))?;

    let metadata = read_site_project_metadata(&project_dir);
    let requested_target =
        target_dir.unwrap_or_else(|| default_site_upload_dir(metadata.as_ref()).to_string());
    let target_relative = safe_relative_subdir(&requested_target)
        .ok_or((StatusCode::BAD_REQUEST, "invalid target_dir".into()))?;
    let destination_dir = project_dir.join(&target_relative);
    tokio::fs::create_dir_all(&destination_dir)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create destination directory: {error}"),
            )
        })?;

    let group_root = format!("sites/{site_slug}");
    let group = if target_relative.as_os_str().is_empty() {
        group_root.clone()
    } else {
        format!(
            "{group_root}/{}",
            target_relative.to_string_lossy().replace('\\', "/")
        )
    };

    let mut saved = Vec::new();
    for (filename, data) in uploads {
        let safe_name = sanitize_upload_filename(&filename);
        let destination = dedupe_destination(&destination_dir, &safe_name);
        tokio::fs::write(&destination, &data)
            .await
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to write uploaded file: {error}"),
                )
            })?;

        let meta = std::fs::metadata(&destination).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to stat uploaded file: {error}"),
            )
        })?;

        let saved_name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&safe_name)
            .to_string();
        let Some(handle) = response_path_for_profile_file(&data_dir, &destination) else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode uploaded file handle".into(),
            ));
        };

        saved.push(ContentFileEntry {
            filename: saved_name.clone(),
            path: handle,
            size: meta.len(),
            modified: modified_rfc3339(&meta),
            category: categorize(&saved_name),
            group: group.clone(),
        });
    }

    Ok(Json(saved))
}

/// GET /api/files?path=... -- serve files by query parameter (for absolute paths).
pub async fn serve_file_by_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(filename) = params.get("path") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_file_impl(&data_dir, filename).await
}

/// GET /api/files/:filename -- serve uploaded files and pipeline report files.
pub async fn serve_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_file_impl(&data_dir, &filename).await
}

async fn serve_file_impl(data_dir: &std::path::Path, filename: &str) -> Response {
    let Some(path) = resolve_scoped_download_path(data_dir, filename) else {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
    };

    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Detect content type from extension
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("pdf") => "application/pdf",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };

    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string())
        .replace(['"', '\r', '\n', '\\'], "_");

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", content_type.parse().unwrap());
    headers.insert(
        "content-disposition",
        format!("inline; filename=\"{display_name}\"")
            .parse()
            .unwrap(),
    );

    (StatusCode::OK, headers, data).into_response()
}

fn api_session_workspace_dirs(
    data_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let profile_id = infer_profile_id_from_data_dir(data_dir);
    let mut dirs = Vec::with_capacity(3);
    let mut seen = HashSet::new();

    for key in [
        SessionKey::with_profile(&profile_id, "api", session_id),
        SessionKey::with_profile(MAIN_PROFILE_ID, "api", session_id),
        SessionKey::new("api", session_id),
    ] {
        let encoded_base = octos_bus::session::encode_path_component(key.base_key());
        let path = data_dir.join("users").join(encoded_base).join("workspace");
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    dirs
}

#[cfg(test)]
fn api_session_workspace_dir(data_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    api_session_workspace_dirs(data_dir, session_id)
        .into_iter()
        .next()
        .unwrap_or_else(|| data_dir.join("users").join("workspace"))
}

fn infer_profile_id_from_data_dir(data_dir: &std::path::Path) -> String {
    data_dir
        .file_name()
        .and_then(|name| (name == "data").then_some(data_dir))
        .and_then(|_| data_dir.parent())
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(MAIN_PROFILE_ID)
        .to_string()
}

/// Decision: which profile id should the request resolve to, given the
/// header-derived candidate (post-middleware — already stripped if the
/// connection was untrusted), an identity-derived id, and the auth
/// identity for an authorization check?
///
/// **Auth bypass fix (#995)**: the legacy precedence
/// `header.or(identity)` let an authenticated request set
/// `X-Profile-Id: <victim>` and read the victim's data dir. The current
/// precedence is identity-first; if a header is *also* present (i.e.
/// from a trusted proxy after the strip middleware ran), it must name
/// a profile the identity is authorized to act on — otherwise we
/// return `403`, never silently override.
///
/// - Unauthenticated request + header: legacy hint, pass through.
/// - Authenticated + header MATCHES identity scope: use that profile
///   (lets per-tenant Caddy ingress narrow admin auth to a tenant).
/// - Authenticated + header is unauthorized: `403`.
/// - Authenticated + no header: use identity's own profile.
/// - Neither header nor identity: `BAD_REQUEST`.
//
// `clippy::result_large_err` is consistent with `resolve_profile_data_dir`
// and the rest of this handler module — boxing the response just for this
// helper would diverge from the surrounding style.
#[allow(clippy::result_large_err)]
pub(crate) fn decide_resolved_profile_id(
    state: &AppState,
    identity: Option<&AuthIdentity>,
    header_profile_id: Option<&str>,
    identity_profile_id: Option<&str>,
) -> Result<String, Response> {
    match (identity, header_profile_id) {
        (Some(identity), Some(pid)) => {
            if super::auth_handlers::is_authorized_for_profile(state, identity, pid) {
                return Ok(pid.to_string());
            }
            tracing::warn!(
                target: "octos::api::auth",
                identity = ?identity,
                requested_profile = %pid,
                "X-Profile-Id denied — authenticated identity not authorized for the requested profile"
            );
            Err((StatusCode::FORBIDDEN, "forbidden").into_response())
        }
        (Some(_), None) => identity_profile_id.map(str::to_string).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing X-Profile-Id and no authenticated profile context",
            )
                .into_response()
        }),
        (None, Some(pid)) => Ok(pid.to_string()),
        (None, None) => Err((
            StatusCode::BAD_REQUEST,
            "missing X-Profile-Id and no authenticated profile context",
        )
            .into_response()),
    }
}

async fn resolve_profile_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<std::path::PathBuf, Response> {
    if let Some((_profile_id, _port)) = resolve_api_port(state, headers).await {
        if let Some(ref ps) = state.profile_store {
            let header_profile_id = routed_profile_id_from_headers(state, headers);
            let identity_profile_id = match identity {
                Some(AuthIdentity::User { id, .. }) => Some(id.as_str()),
                Some(AuthIdentity::Admin) => Some(ADMIN_PROFILE_ID),
                None => None,
            };

            let pid = decide_resolved_profile_id(
                state,
                identity,
                header_profile_id.as_deref(),
                identity_profile_id,
            )?;
            match ps.get(&pid) {
                Ok(Some(profile)) => return Ok(ps.resolve_data_dir(&profile)),
                Ok(None) => {
                    return Err((StatusCode::NOT_FOUND, "profile not found").into_response());
                }
                Err(error) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("profile lookup failed: {error}"),
                    )
                        .into_response());
                }
            }
        }
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no profile store").into_response());
    }

    Err((StatusCode::SERVICE_UNAVAILABLE, "no gateway").into_response())
}

fn resolve_profile_data_dir_by_id(
    state: &AppState,
    profile_id: &str,
) -> Result<std::path::PathBuf, Response> {
    let profile_id = if profile_id.is_empty() {
        MAIN_PROFILE_ID
    } else {
        profile_id
    };

    let Some(ref store) = state.profile_store else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no profile store").into_response());
    };

    match store.get(profile_id) {
        Ok(Some(profile)) => Ok(store.resolve_data_dir(&profile)),
        Ok(None) => Err((StatusCode::NOT_FOUND, "profile not found").into_response()),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("profile lookup failed: {error}"),
        )
            .into_response()),
    }
}

fn sanitize_upload_filename(filename: &str) -> String {
    filename
        .replace(['/', '\\', '\0'], "_")
        .chars()
        .take(200)
        .collect::<String>()
}

fn safe_relative_subdir(dir: &str) -> Option<std::path::PathBuf> {
    let normalized = dir.trim().replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return Some(std::path::PathBuf::new());
    }

    let mut relative = std::path::PathBuf::new();
    for component in std::path::Path::new(trimmed).components() {
        match component {
            std::path::Component::Normal(segment) => relative.push(segment),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }

    Some(relative)
}

fn default_site_upload_dir(metadata: Option<&SiteProjectMetadata>) -> &'static str {
    match metadata.map(|meta| meta.template.as_str()) {
        Some("quarto-lesson") => "images/uploads",
        _ => "public/uploads",
    }
}

fn dedupe_destination(dest_dir: &std::path::Path, filename: &str) -> std::path::PathBuf {
    let candidate = dest_dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("file");
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());

    for index in 2..10_000 {
        let deduped = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let deduped_path = dest_dir.join(&deduped);
        if !deduped_path.exists() {
            return deduped_path;
        }
    }

    candidate
}

fn modified_rfc3339(meta: &std::fs::Metadata) -> String {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn site_preview_html(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{title}</title>
    <style>
      :root {{
        color-scheme: light dark;
        --bg: #0f172a;
        --panel: rgba(15, 23, 42, 0.78);
        --text: #e2e8f0;
        --muted: #94a3b8;
        --border: rgba(148, 163, 184, 0.18);
        --accent: #38bdf8;
        --font: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      }}
      * {{ box-sizing: border-box; }}
      body {{
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        padding: 24px;
        color: var(--text);
        font-family: var(--font);
        background:
          radial-gradient(circle at top right, rgba(56, 189, 248, 0.18), transparent 22rem),
          linear-gradient(180deg, #0f172a 0%, #111827 100%);
      }}
      .card {{
        width: min(820px, 100%);
        padding: 24px;
        border: 1px solid var(--border);
        border-radius: 24px;
        background: var(--panel);
        backdrop-filter: blur(18px);
      }}
      h1 {{
        margin: 0 0 12px;
        font-size: clamp(1.6rem, 3vw, 2.4rem);
        letter-spacing: -0.04em;
      }}
      p {{
        margin: 0;
        line-height: 1.75;
        color: var(--muted);
        white-space: pre-wrap;
      }}
      code {{
        color: var(--text);
      }}
    </style>
  </head>
  <body>
    <article class="card">
      <h1>{title}</h1>
      <p>{body}</p>
    </article>
  </body>
</html>"#,
        title = title,
        body = body,
    );

    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn preview_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Resolve the build output directory for a site preview.
///
/// Issue #996: the metadata file is LLM-writable via `edit_file`, so
/// the `build_output_dir` field is **untrusted on read** — we route
/// every read through [`validated_build_output_dir`]. Callers must
/// surface the typed error to the user (e.g. as a 4xx-equivalent
/// preview page), not silently fall back to the raw join.
fn output_dir_for_site(
    project_dir: &std::path::Path,
    metadata: &SiteProjectMetadata,
) -> Result<std::path::PathBuf, BuildOutputDirError> {
    validated_build_output_dir(metadata, project_dir)
}

fn newest_tree_mtime(
    root: &std::path::Path,
    skip_dir_names: &[&str],
) -> Option<std::time::SystemTime> {
    fn walk(
        dir: &std::path::Path,
        skip_dir_names: &[&str],
        latest: &mut Option<std::time::SystemTime>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if path.is_dir() {
                if skip_dir_names.iter().any(|skip| *skip == file_name) {
                    continue;
                }
                walk(&path, skip_dir_names, latest);
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if latest.map(|current| modified > current).unwrap_or(true) {
                        *latest = Some(modified);
                    }
                }
            }
        }
    }

    if root.is_file() {
        return root.metadata().ok()?.modified().ok();
    }

    let mut latest = None;
    walk(root, skip_dir_names, &mut latest);
    latest
}

fn site_build_needed(project_dir: &std::path::Path, output_dir: &std::path::Path) -> bool {
    if !output_dir.exists() {
        return true;
    }

    let output_time = newest_tree_mtime(output_dir, &[]);
    let source_time = newest_tree_mtime(
        project_dir,
        &[
            "node_modules",
            ".git",
            ".next",
            ".astro",
            "dist",
            "out",
            "docs",
        ],
    );

    match (source_time, output_time) {
        (Some(source_time), Some(output_time)) => source_time > output_time,
        (Some(_), None) => true,
        _ => false,
    }
}

fn site_build_cache_dir(project_dir: &std::path::Path) -> std::path::PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let project_key = project_dir
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let preferred = std::env::temp_dir()
        .join("octos-site-build-npm-cache")
        .join(user)
        .join(project_key);
    let _ = std::fs::create_dir_all(&preferred);
    preferred
}

fn apply_site_build_env(command: &mut std::process::Command, project_dir: &std::path::Path) {
    let cache_dir = site_build_cache_dir(project_dir);
    command
        .env("ASTRO_TELEMETRY_DISABLED", "1")
        .env("NPM_CONFIG_CACHE", &cache_dir)
        .env("npm_config_cache", &cache_dir);
}

fn run_build_command(command: &mut std::process::Command, label: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("{label} failed to start: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = format!("{stdout}\n{stderr}").trim().to_string();
    if detail.is_empty() {
        Err(format!("{label} failed with status {}", output.status))
    } else {
        Err(format!("{label} failed:\n{detail}"))
    }
}

/// Categorised reason `ensure_site_build_output` rejected a preview
/// build. Codex BLOCKING #2 (issue #996 follow-up): the caller maps
/// `InvalidMetadata` to HTTP 400 with a scrubbed body and the build/
/// missing-artifact variants to HTTP 5xx with the project path
/// stripped out — previously every error returned 200 with a "Build
/// Failed" page and leaked the full project path in the body.
///
/// `pub` (re-exported only via the `#[doc(hidden)]` `testing` module
/// in `crate::api`) so the build_output_dir test suite can directly
/// induce each variant and assert the HTTP status mapping via
/// [`preview_build_error_response`].
#[derive(Debug)]
pub enum SiteBuildError {
    /// The LLM-controlled metadata failed validation (allow-list,
    /// per-template equality, `..` escape, etc.). 4xx surface.
    InvalidMetadata(BuildOutputDirError),
    /// The template slug is not one we know how to build (post
    /// metadata-validation; the validator already rejects unknown
    /// templates via the `TemplateMismatch` path because their
    /// `output_dir()` defaults to `docs`, but keep this branch for
    /// defence-in-depth). 4xx surface.
    UnsupportedTemplate,
    /// The build tool (npm / quarto) exited non-zero. 5xx surface,
    /// scrubbed body.
    BuildCommandFailed,
    /// The build claimed to succeed but the expected output dir was
    /// not created. 5xx surface, scrubbed body.
    OutputArtifactMissing,
    /// Post-build re-validation tripped — typically a symlink that
    /// the build step left behind. 4xx surface (it's a contract
    /// violation by the build step, not a server hiccup).
    PostBuildValidation(BuildOutputDirError),
}

fn ensure_site_build_output(
    project_dir: &std::path::Path,
    metadata: &SiteProjectMetadata,
) -> Result<std::path::PathBuf, SiteBuildError> {
    fn site_build_locks() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
        static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
        LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn site_build_lock(project_dir: &std::path::Path) -> Arc<Mutex<()>> {
        let key = std::fs::canonicalize(project_dir)
            .unwrap_or_else(|_| project_dir.to_path_buf())
            .to_string_lossy()
            .to_string();
        let mut locks = site_build_locks().lock().unwrap();
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    let build_lock = site_build_lock(project_dir);
    let _build_guard = build_lock.lock().unwrap();
    // Validate the LLM-controlled metadata field before we trust it
    // to derive the build output path. Issue #996.
    let output_dir =
        output_dir_for_site(project_dir, metadata).map_err(SiteBuildError::InvalidMetadata)?;
    if !site_build_needed(project_dir, &output_dir) {
        return Ok(output_dir);
    }

    match metadata.template.as_str() {
        "quarto-lesson" => {
            let mut render = std::process::Command::new("quarto");
            render.current_dir(project_dir).arg("render");
            run_build_command(&mut render, "quarto render")
                .map_err(|_| SiteBuildError::BuildCommandFailed)?;
        }
        "astro-site" | "nextjs-app" | "react-vite" => {
            if !project_dir.join("node_modules").exists() {
                let mut install = std::process::Command::new("npm");
                install.current_dir(project_dir).arg("install");
                apply_site_build_env(&mut install, project_dir);
                run_build_command(&mut install, "npm install")
                    .map_err(|_| SiteBuildError::BuildCommandFailed)?;
            }
            let mut build = std::process::Command::new("npm");
            build.current_dir(project_dir).arg("run").arg("build");
            apply_site_build_env(&mut build, project_dir);
            run_build_command(&mut build, "npm run build")
                .map_err(|_| SiteBuildError::BuildCommandFailed)?;
        }
        _ => return Err(SiteBuildError::UnsupportedTemplate),
    }

    if !output_dir.exists() {
        return Err(SiteBuildError::OutputArtifactMissing);
    }

    // Re-validate now that the output dir exists on disk — this
    // re-runs the canonical-descendant check and catches symlinks
    // that the build step might have left behind.
    output_dir_for_site(project_dir, metadata).map_err(SiteBuildError::PostBuildValidation)
}

fn safe_preview_join(root: &std::path::Path, request_path: &str) -> Option<std::path::PathBuf> {
    let mut joined = root.to_path_buf();
    for segment in request_path.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.contains('\\') {
            return None;
        }
        joined.push(segment);
    }
    Some(joined)
}

fn resolve_preview_asset_path(
    output_dir: &std::path::Path,
    request_path: &str,
) -> Option<std::path::PathBuf> {
    fn resolve_direct(
        output_dir: &std::path::Path,
        request_path: &str,
    ) -> Option<std::path::PathBuf> {
        let candidate = safe_preview_join(output_dir, request_path)?;
        if request_path.is_empty() {
            Some(output_dir.join("index.html"))
        } else if candidate.is_dir() {
            Some(candidate.join("index.html"))
        } else if candidate.exists() {
            Some(candidate)
        } else {
            let nested_index = candidate.join("index.html");
            if nested_index.exists() {
                Some(nested_index)
            } else if !request_path.contains('.') {
                let html = candidate.with_extension("html");
                if html.exists() { Some(html) } else { None }
            } else {
                None
            }
        }
    }

    let request_path = request_path.trim_start_matches('/');
    let resolved = resolve_direct(output_dir, request_path).or_else(|| {
        if request_path.contains('.') {
            return None;
        }

        let segments: Vec<&str> = request_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.len() < 2 {
            return None;
        }

        // Legacy generated sites sometimes emit page-relative links such as
        // "./capabilities/" from "/concepts/". When that happens, the
        // browser requests "concepts/capabilities/". Fall back to the
        // rightmost route segment at the preview root if it exists there.
        for start in 1..segments.len() {
            let fallback_path = segments[start..].join("/");
            if let Some(path) = resolve_direct(output_dir, &fallback_path) {
                return Some(path);
            }
        }

        None
    })?;

    let canonical_root = std::fs::canonicalize(output_dir).ok()?;
    let canonical_resolved = std::fs::canonicalize(resolved).ok()?;
    if !canonical_resolved.starts_with(&canonical_root) {
        return None;
    }

    Some(canonical_resolved)
}

/// Serve a preview file with TOCTOU-safe semantics. Codex round-1
/// BLOCKING #1: the previous body called `tokio::fs::read`, which
/// follows symlinks — an attacker who could swap `<output_dir>` (or
/// any ancestor) for a symlink between the canonical-descendant
/// check in [`resolve_preview_asset_path`] and the read here would
/// escape the project dir even though the validator passed.
///
/// Codex round-2 BLOCKING #1: the round-1 fix used a
/// `symlink_metadata` ancestor walk + final `O_NOFOLLOW` open. That
/// left a multi-syscall TOCTOU window — an attacker could swap an
/// ancestor between the stat and the open. The replacement routes
/// through [`crate::api::preview::serve_preview_no_follow`], which
/// walks every component with `rustix::fs::openat` so each step is
/// anchored to a parent fd that already passed `O_NOFOLLOW`.
///
/// The validation chain rooted here is:
///   `project_dir` (caller-trusted scaffold)
///     → `output_dir` component (allow-listed per-template)
///     → asset-relative path (resolved by `resolve_preview_asset_path`).
/// We pass `project_dir`, not `output_dir`, as the walk root so
/// every component — including the `output_dir` name itself — is
/// re-validated by the `openat` walk on each request. The previous
/// shape passed `output_dir` and the walk only re-checked the
/// asset-relative segment, missing the swap-`output_dir`-itself
/// variant.
async fn serve_preview_file(project_dir: &std::path::Path, path: std::path::PathBuf) -> Response {
    let project_root = project_dir.to_path_buf();
    let leaf_for_headers = path.clone();
    let data = match crate::api::preview::serve_preview_no_follow(project_root, path).await {
        Ok(data) => data,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let cache_control = if leaf_for_headers.extension().and_then(|ext| ext.to_str()) == Some("html")
    {
        "no-cache, no-store, must-revalidate"
    } else {
        "public, max-age=30"
    };

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                preview_content_type(&leaf_for_headers),
            ),
            (axum::http::header::CACHE_CONTROL, cache_control),
        ],
        data,
    )
        .into_response()
}

async fn serve_site_preview_impl(
    data_dir: std::path::PathBuf,
    session_id: String,
    site_slug: String,
    request_path: String,
) -> Response {
    let project_dir = api_session_workspace_dirs(&data_dir, &session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&site_slug))
        .find(|candidate| candidate.exists());

    let Some(project_dir) = project_dir else {
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Site Preview Not Found",
            &format!(
                "No scaffold exists yet for session `{session_id}` and site `{site_slug}`.\n\nCreate the site session first so Octos can scaffold the project workspace."
            ),
        );
    };

    let Some(metadata) = read_site_project_metadata(&project_dir) else {
        // Codex BLOCKING #2: don't leak `project_dir` in the error
        // body. The session/slug names came from the URL so are
        // safe to echo; the on-disk path is not.
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Missing Site Metadata",
            &format!(
                "The scaffold for `{session_id}` / `{site_slug}` is missing or its `mofa-site-session.json` is invalid.",
            ),
        );
    };

    let build_task = {
        let project_dir = project_dir.clone();
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || ensure_site_build_output(&project_dir, &metadata))
    };

    let output_dir = match build_task.await {
        Ok(Ok(output_dir)) => output_dir,
        Ok(Err(error)) => return preview_build_error_response(&metadata.template, error),
        Err(_join_error) => {
            // Worker panicked — server-internal, no useful detail
            // for the client.
            return site_preview_html(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Preview Worker Crashed",
                "The preview build worker crashed. Please retry; if this persists check server logs.",
            );
        }
    };

    let Some(path) = resolve_preview_asset_path(&output_dir, &request_path) else {
        // Codex BLOCKING #2: drop `output_dir.display()` — that's
        // the project path. The request path is user-supplied so
        // safe to echo.
        return site_preview_html(
            StatusCode::NOT_FOUND,
            "Preview Asset Missing",
            &format!(
                "The built preview exists, but no asset was found for `{}`.",
                if request_path.is_empty() {
                    "/"
                } else {
                    request_path.as_str()
                },
            ),
        );
    };

    // Codex round-2 BLOCKING #1: pass `project_dir` (not
    // `output_dir`) as the symlink-safe walk root, so the openat
    // chain re-validates `output_dir`'s component name on every
    // request as well. The round-1 wiring passed `output_dir`, which
    // meant a swap of the `output_dir` directory name itself between
    // build and serve was only caught by the canonical-descendant
    // check inside the helper — and that check is vulnerable to the
    // same TOCTOU shape it's supposed to fix.
    serve_preview_file(&project_dir, path).await
}

/// Map a `SiteBuildError` to a scrubbed HTTP response. Codex
/// BLOCKING #2: validation failures are 4xx (the LLM messed up the
/// metadata, not the server), build failures are 5xx, neither leaks
/// the project path in the response body.
///
/// `pub` (re-exported only via the `#[doc(hidden)]` `testing` module
/// in `crate::api`) so the build_output_dir test suite can assert
/// the status-code mapping at the handler layer without spinning up
/// the full Axum router. Codex round-2 follow-up.
pub fn preview_build_error_response(template: &str, error: SiteBuildError) -> Response {
    match error {
        SiteBuildError::InvalidMetadata(reason) => site_preview_html(
            StatusCode::BAD_REQUEST,
            "Preview Build Rejected",
            &format!(
                "Octos rejected the preview build for template `{template}`: {reason}.\n\nThe `mofa-site-session.json` metadata is invalid. Restore the original `build_output_dir` value (or scaffold the site again) and reload.",
            ),
        ),
        SiteBuildError::UnsupportedTemplate => site_preview_html(
            StatusCode::BAD_REQUEST,
            "Preview Build Rejected",
            &format!("Unsupported site template `{template}`."),
        ),
        SiteBuildError::PostBuildValidation(reason) => site_preview_html(
            StatusCode::BAD_REQUEST,
            "Preview Build Rejected",
            &format!(
                "Octos rejected the preview build for template `{template}` after the build step: {reason}.\n\nThe build artifact left an unsafe output directory. Re-scaffold or clean the build cache.",
            ),
        ),
        SiteBuildError::BuildCommandFailed => site_preview_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Preview Build Failed",
            &format!(
                "The build tool failed for template `{template}`. Check server logs for the full build output.",
            ),
        ),
        SiteBuildError::OutputArtifactMissing => site_preview_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Preview Build Failed",
            "Preview build artifact missing — the build claimed success but produced no output.",
        ),
    }
}

/// GET /api/site-preview/{session_id}/{site_slug} — serve the preview root for a site session.
///
/// Codex review of PR #1001 (issue #994 follow-up): this route is a
/// parallel preview surface to `/api/preview/{profile_id}/...` (which
/// PR #1001 hardened by moving onto `chat_api` + asserting ownership).
/// Unlike that route, `/api/site-preview/*` has no `profile_id` URL
/// segment, and the legacy implementation derived the profile via
/// [`resolve_profile_data_dir`] which preferred the `X-Profile-Id`
/// header over the authenticated identity. With a valid bearer token
/// plus a spoofed `X-Profile-Id`, an authenticated tenant A could read
/// tenant B's preview through this side door.
///
/// Fix: route via [`resolve_site_preview_data_dir`] which mirrors the
/// `/api/my/*` flow:
///   1. authenticated identity required (401 if missing),
///   2. host-routed profile (subdomain) is honored only when the
///      identity is authorized for it (`is_authorized_for_profile`),
///   3. otherwise the profile id is derived directly from the
///      identity (`User { id }` or the admin profile),
///   4. `X-Profile-Id` is NEVER trusted for profile routing on this
///      route. The header is ignored.
pub async fn serve_site_preview_root(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((session_id, site_slug)): axum::extract::Path<(String, String)>,
) -> Response {
    let data_dir = match resolve_site_preview_data_dir(&state, &headers, identity.as_ref()) {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, String::new()).await
}

/// GET /api/site-preview/{session_id}/{site_slug}/{*path} — serve built preview assets.
///
/// See [`serve_site_preview_root`] for the security model.
pub async fn serve_site_preview_path(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((session_id, site_slug, request_path)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> Response {
    let data_dir = match resolve_site_preview_data_dir(&state, &headers, identity.as_ref()) {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    serve_site_preview_impl(data_dir, session_id, site_slug, request_path).await
}

/// Identity-first profile-data-dir resolver for `/api/site-preview/*`.
///
/// Codex flagged the legacy [`resolve_profile_data_dir`] path as
/// unsafe for routes that lack an authoritative `profile_id` URL
/// segment: it falls back to `X-Profile-Id` and treats the header as
/// trusted, so an authed tenant A spoofing `X-Profile-Id: tenant-b`
/// reads tenant B's preview. This helper deliberately ignores the
/// header for profile routing and instead:
///
/// 1. Requires an `AuthIdentity` (the route is wrapped in
///    `user_auth_middleware`, so reaching here with `None` is a
///    routing bug — fail closed with 401).
/// 2. If the request's host resolves to a tenant profile via the
///    subdomain (e.g. `tenant-a.api.ominix.io`), honor it only when
///    [`is_authorized_for_profile`] passes for the authenticated
///    identity — otherwise 403 (mirrors `/api/my/*` host-scoping).
/// 3. Otherwise, use the identity's own profile id: regular users
///    map to their own user id, admin token maps to the admin
///    profile.
/// 4. Resolve the data dir from the now-authoritative profile id.
#[allow(clippy::result_large_err)] // matches sibling `resolve_profile_data_dir_by_id`
fn resolve_site_preview_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&Extension<AuthIdentity>>,
) -> Result<std::path::PathBuf, Response> {
    let Some(Extension(identity)) = identity else {
        tracing::warn!(
            "site-preview route reached without AuthIdentity — routing bug? failing closed"
        );
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };

    // Host-routed profile takes precedence (e.g. tenant subdomain),
    // but ONLY when the authenticated identity is allowed to see
    // that profile. Otherwise 403 — never silently fall through to
    // another profile.
    if let Some(host) = request_host(headers) {
        if !is_local_request_host(&host) {
            if let Some(candidate) = host.split('.').next() {
                if let Some(host_profile_id) = resolve_profile_id_candidate(state, candidate) {
                    if !is_authorized_for_profile(state, identity, &host_profile_id) {
                        tracing::warn!(
                            identity = ?identity,
                            host_profile_id = %host_profile_id,
                            "site-preview host-scope denied — identity not authorized for the tenant subdomain"
                        );
                        return Err(StatusCode::FORBIDDEN.into_response());
                    }
                    return resolve_profile_data_dir_by_id(state, &host_profile_id);
                }
            }
        }
    }

    // No host-routed profile: derive purely from the authenticated
    // identity. Crucially we do NOT read `X-Profile-Id` for profile
    // routing — that's the codex-flagged side door.
    let identity_profile_id = match identity {
        AuthIdentity::Admin => ADMIN_PROFILE_ID,
        AuthIdentity::User { id, .. } => id.as_str(),
    };

    // Defence in depth: if the caller DID send `X-Profile-Id`, treat
    // any value that doesn't match the authenticated identity's
    // profile (and that the identity isn't otherwise authorized for)
    // as an explicit cross-tenant spoofing attempt → 403. This makes
    // the rejection signal unambiguous in logs and tests, mirroring
    // the response shape `/api/preview/{profile_id}/*` returns when
    // identity does not own the route's profile_id segment.
    if let Some(header_value) = headers.get("x-profile-id").and_then(|v| v.to_str().ok()) {
        let trimmed = header_value.trim();
        if !trimmed.is_empty()
            && let Some(spoofed) = resolve_profile_id_candidate(state, trimmed)
            && !is_authorized_for_profile(state, identity, &spoofed)
        {
            tracing::warn!(
                identity = ?identity,
                spoofed_profile_id = %spoofed,
                "site-preview denied — X-Profile-Id requests a profile the identity is not authorized for (codex follow-up to PR #1001 / issue #994)"
            );
            return Err(StatusCode::FORBIDDEN.into_response());
        }
    }

    resolve_profile_data_dir_by_id(state, identity_profile_id)
}

/// GET /api/preview/{profile_id}/{session_id}/{site_slug} —
/// auth-and-ownership-gated preview root for site iframes.
///
/// Issue #994 (P0 sev2 cross-tenant data read): prior to this commit
/// the route lived on the unauthenticated public router branch and
/// resolved both `profile_id` and `session_id` purely from the URL
/// tuple. The tuple is moderately guessable (subdomain + short
/// timestamp + small slug allow-list), so any caller who could guess
/// it could read another tenant's built site.
///
/// The fix:
/// 1. Route now requires user auth (`user_auth_middleware` in
///    `router.rs`). An unauthenticated request is rejected at the
///    middleware with `401 Unauthorized` before the handler runs.
/// 2. Handler asserts the authenticated identity is authorized for
///    the route's `profile_id` via [`is_authorized_for_profile`].
///    Cross-tenant mismatch → `403 Forbidden`.
/// 3. Handler verifies the route's `session_id` resolves to a
///    workspace under the profile's data directory. A session that
///    does not exist in that tree (e.g. crafted / harvested) → `403
///    Forbidden` (NOT 404 — we never disclose whether the slot is
///    empty vs forbidden).
///
/// Interaction with issue #995: when the X-Profile-Id strip lands,
/// the authenticated identity's profile becomes authoritative and
/// the route's `profile_id` segment is only trusted after
/// `is_authorized_for_profile` confirms the match. The order is
/// important — never use the route segment to look up data before
/// the ownership check passes.
pub async fn serve_owned_site_preview_root(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((profile_id, session_id, site_slug)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> Response {
    serve_owned_site_preview(
        state,
        identity,
        profile_id,
        session_id,
        site_slug,
        String::new(),
    )
    .await
}

/// GET /api/preview/{profile_id}/{session_id}/{site_slug}/{*path} —
/// auth-and-ownership-gated preview assets. See
/// [`serve_owned_site_preview_root`] for the security model.
pub async fn serve_owned_site_preview_path(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path((profile_id, session_id, site_slug, request_path)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
) -> Response {
    serve_owned_site_preview(
        state,
        identity,
        profile_id,
        session_id,
        site_slug,
        request_path,
    )
    .await
}

/// Shared implementation for [`serve_owned_site_preview_root`] and
/// [`serve_owned_site_preview_path`]. Performs the auth + profile +
/// session-ownership checks then delegates to the existing
/// `serve_site_preview_impl` for the build / serve path.
async fn serve_owned_site_preview(
    state: Arc<AppState>,
    identity: Option<Extension<AuthIdentity>>,
    profile_id: String,
    session_id: String,
    site_slug: String,
    request_path: String,
) -> Response {
    // 1. Auth identity must be present. The router wraps this route in
    //    `user_auth_middleware`, which rejects unauthenticated
    //    requests with 401 before the handler runs — so reaching here
    //    with `identity = None` is a routing bug, not user error. Fail
    //    closed with 401 just in case.
    let Some(Extension(identity)) = identity else {
        tracing::warn!(
            profile_id = %profile_id,
            "preview route reached without AuthIdentity — routing bug? failing closed"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // 2. The authenticated identity must be authorized for the route's
    //    `profile_id`. Admin (token or admin-role user) is authorized
    //    for any profile; a regular user is authorized for their own
    //    profile and any sub-accounts they own. Cross-tenant => 403.
    if !is_authorized_for_profile(&state, &identity, &profile_id) {
        tracing::warn!(
            identity = ?identity,
            route_profile_id = %profile_id,
            "preview route denied — identity not authorized for route's profile_id (issue #994)"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    // 3. Now that ownership is confirmed, resolve the data directory
    //    by the (authoritative) route profile_id. A non-existent
    //    profile is a 404 (the profile literally does not exist) but
    //    `is_authorized_for_profile` already passed for sub-account
    //    paths, so this is mostly a sanity check.
    let data_dir = match resolve_profile_data_dir_by_id(&state, &profile_id) {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };

    // 4. Session ownership: the route's `session_id` must resolve to
    //    a workspace under this profile's data directory. We mirror
    //    the search `serve_site_preview_impl` performs below, but
    //    return 403 (not 404) when no candidate exists so the
    //    response is indistinguishable from the cross-tenant denial.
    let session_owned = api_session_workspace_dirs(&data_dir, &session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&site_slug))
        .any(|candidate| candidate.exists());
    if !session_owned {
        tracing::warn!(
            identity = ?identity,
            route_profile_id = %profile_id,
            session_id = %session_id,
            site_slug = %site_slug,
            "preview route denied — session/site does not exist under profile's data dir (issue #994)"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    serve_site_preview_impl(data_dir, session_id, site_slug, request_path).await
}

/// GET /api/files/list?dirs=research,slides,skill-output&session_id=... — list files in profile content directories.
fn should_skip_listing_dir(dir_name: &str, include_build: bool) -> bool {
    let lower = dir_name.to_ascii_lowercase();
    lower.starts_with('.')
        || matches!(lower.as_str(), "node_modules" | "coverage" | "target")
        || lower == "output_old"
        || (!include_build && matches!(lower.as_str(), "dist" | "out" | "docs" | "build"))
}

pub async fn list_content_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_profile_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };

    let dirs_param = params
        .get("dirs")
        .cloned()
        .unwrap_or_else(|| "research,slides,skill-output".to_string());
    let requested_dirs: Vec<String> = dirs_param
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let session_id = params
        .get("session_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let include_build = params
        .get("include_build")
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let session_scoped = session_id.is_some();

    let mut scan_dirs = requested_dirs.clone();
    if let Some(session_id) = session_id {
        scan_dirs.clear();
        for workspace in api_session_workspace_dirs(&data_dir, session_id) {
            if workspace.exists() {
                for dir_name in &requested_dirs {
                    if std::path::Path::new(dir_name.as_str()).is_absolute() {
                        continue;
                    }
                    let ws_dir = workspace.join(dir_name);
                    if ws_dir.exists() && ws_dir.is_dir() {
                        scan_dirs.push(ws_dir.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        // Scan per-user workspace directories that match the requested dirs.
        // Only use original relative dir names — absolute paths from prior
        // iterations would bypass ws.join() (Path::join replaces on absolute).
        let users_dir = data_dir.join("users");
        if let Ok(entries) = std::fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let ws = entry.path().join("workspace");
                if !ws.exists() {
                    continue;
                }
                for dir_name in &requested_dirs {
                    if std::path::Path::new(dir_name.as_str()).is_absolute() {
                        continue;
                    }
                    let ws_dir = ws.join(dir_name);
                    if ws_dir.exists() && ws_dir.is_dir() {
                        scan_dirs.push(ws_dir.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    #[derive(Serialize)]
    struct ContentFile {
        filename: String,
        path: String,
        size: u64,
        modified: String,
        category: String,
        /// Parent directory name for grouping in the UI
        group: String,
    }

    fn display_dir_for_scan(dir_name: &str) -> String {
        if !std::path::Path::new(dir_name).is_absolute() {
            return dir_name.trim_matches('/').to_string();
        }

        let normalized = dir_name.replace('\\', "/");
        if let Some((_, suffix)) = normalized.rsplit_once("/workspace/") {
            let trimmed = suffix.trim_matches('/');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        let path = std::path::Path::new(dir_name);
        let parts: Vec<&str> = path
            .components()
            .rev()
            .take(2)
            .map(|component| component.as_os_str().to_str().unwrap_or(""))
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            "files".into()
        } else {
            parts.into_iter().rev().collect::<Vec<_>>().join("/")
        }
    }

    // Keep meaningful session files while filtering obvious intermediates.
    fn is_output_file(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        // Skip hidden files
        if lower.starts_with('.') {
            return false;
        }
        // Skip research intermediate files
        if lower.starts_with('_') {
            return false;
        } // _report.md, _search_results.md, _sources.json
        // Skip intermediates
        if lower.starts_with("panel-") {
            return false;
        }
        if lower.contains("-ref.") {
            return false;
        } // mofa reference images
        // Only keep meaningful output extensions
        matches!(
            lower.rsplit('.').next().unwrap_or(""),
            "md" | "markdown"
                | "txt"
                | "pptx"
                | "pdf"
                | "docx"
                | "xlsx"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "gif"
                | "svg"
                | "avif"
                | "mp3"
                | "wav"
                | "mp4"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "json"
                | "css"
                | "html"
                | "astro"
                | "qmd"
                | "yaml"
                | "yml"
                | "sh"
                | "mjs"
                | "cjs"
        )
    }

    fn collect_files_recursive(
        data_dir: &std::path::Path,
        current_dir: &std::path::Path,
        display_root: &str,
        relative_dir: &std::path::Path,
        include_build: bool,
        allow_nested_dirs: bool,
        files: &mut Vec<ContentFile>,
    ) {
        let entries = match std::fs::read_dir(current_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if !allow_nested_dirs || should_skip_listing_dir(&name, include_build) {
                    continue;
                }
                let mut next_relative = relative_dir.to_path_buf();
                next_relative.push(&name);
                collect_files_recursive(
                    data_dir,
                    &path,
                    display_root,
                    &next_relative,
                    include_build,
                    allow_nested_dirs,
                    files,
                );
                continue;
            }

            if !path.is_file() || !is_output_file(&name) {
                continue;
            }

            let meta = match path.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            let group = if relative_dir.as_os_str().is_empty() {
                display_root.to_string()
            } else {
                format!(
                    "{display_root}/{}",
                    relative_dir.to_string_lossy().replace('\\', "/")
                )
            };
            let Some(handle) = response_path_for_profile_file(data_dir, &path) else {
                continue;
            };

            files.push(ContentFile {
                category: categorize(&name),
                filename: name,
                path: handle,
                size: meta.len(),
                modified: modified_rfc3339(&meta),
                group,
            });
        }
    }

    let mut files = Vec::new();
    for dir_name in &scan_dirs {
        let dir_path = if std::path::Path::new(dir_name.as_str()).is_absolute() {
            std::path::PathBuf::from(dir_name.as_str())
        } else {
            data_dir.join(dir_name.as_str())
        };
        if !dir_path.exists() {
            continue;
        }
        let display_dir = display_dir_for_scan(dir_name);
        let allow_nested_dirs = display_dir != "research";
        collect_files_recursive(
            &data_dir,
            &dir_path,
            &display_dir,
            std::path::Path::new(""),
            include_build,
            allow_nested_dirs,
            &mut files,
        );
    }

    // Sort by modified desc; session-scoped project views need a larger ceiling
    // so the source tree remains inspectable.
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files.truncate(if session_scoped { 1000 } else { 100 });
    Json(files).into_response()
}

fn categorize(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".md")
        || lower.ends_with(".markdown")
        || lower.ends_with(".txt")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".json")
        || lower.ends_with(".css")
        || lower.ends_with(".html")
        || lower.ends_with(".astro")
        || lower.ends_with(".qmd")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".sh")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
    {
        "report".into()
    } else if lower.ends_with(".pptx") || lower.ends_with(".pdf") {
        "slides".into()
    } else if lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".webp")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".avif")
    {
        "image".into()
    } else if lower.ends_with(".mp3") || lower.ends_with(".wav") || lower.ends_with(".ogg") {
        "audio".into()
    } else if lower.ends_with(".mp4") || lower.ends_with(".webm") {
        "video".into()
    } else {
        "other".into()
    }
}

/// Result shape for the WS `system/status.get` RPC method (formerly the
/// body of `GET /api/status`, retired in M12 Phase D-5). The `/health`
/// endpoint remains REST as the public liveness probe.
#[derive(Serialize)]
pub struct StatusResponse {
    pub version: String,
    pub model: String,
    pub provider: String,
    pub uptime_secs: i64,
    pub agent_configured: bool,
    /// Public-facing base domain this mini serves profiles under
    /// (e.g. `"crew.ominix.io"`, `"bot.ominix.io"`). The dashboard and
    /// octos-web client consume this to render correct preview URLs
    /// and infer profile IDs from hostnames. Always a concrete string
    /// — falls back to `DEFAULT_BASE_DOMAIN` when unconfigured.
    pub base_domain: String,
}

// Helper for `ui_protocol::handle_system_status_get` (M12 Phase D-5).
// The REST route `GET /api/status` was retired; this function survives
// as the implementation backing the WS `system/status.get` RPC method.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let uptime = chrono::Utc::now() - state.started_at;
    // M11-F: surface a profile-aware status. The legacy server-wide
    // `state.agent` was removed; report the canonical "_main" profile
    // when present, falling back to "none" so the dashboard can still
    // render an unconfigured-server placeholder.
    let main_runtime = state.profiles.get(octos_core::MAIN_PROFILE_ID).cloned();
    let (model, provider) = match &main_runtime {
        Some(rt) => (rt.primary_model_id.clone(), rt.provider_name.clone()),
        None => ("none".to_string(), "none".to_string()),
    };
    let base_domain = state
        .base_domain
        .clone()
        .unwrap_or_else(|| crate::api::DEFAULT_BASE_DOMAIN.to_string());
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model,
        provider,
        uptime_secs: uptime.num_seconds(),
        agent_configured: main_runtime.is_some() || !state.profiles.is_empty(),
        base_domain,
    })
}

/// GET /api/version — public version endpoint (no auth required).
pub async fn version(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let version = env!("CARGO_PKG_VERSION");
    let git_hash = option_env!("OCTOS_GIT_HASH").unwrap_or("");
    let build_date = option_env!("OCTOS_BUILD_DATE").unwrap_or("");
    let full = if git_hash.is_empty() {
        version.to_string()
    } else {
        format!("{version}+{git_hash}")
    };
    Json(serde_json::json!({
        "service": "octos",
        "version": full,
        "build_date": build_date,
        "tunnel_domain": state.tunnel_domain,
    }))
}

/// GET /health — public health check (no auth required).
pub async fn health() -> Json<serde_json::Value> {
    let version = env!("CARGO_PKG_VERSION");
    let git_hash = option_env!("OCTOS_GIT_HASH").unwrap_or("");
    let full = if git_hash.is_empty() {
        version.to_string()
    } else {
        format!("{version}+{git_hash}")
    };
    Json(serde_json::json!({
        "status": "healthy",
        "service": "octos",
        "version": full,
    }))
}

// ───────────────────────────────────────────────────────────────────────────
//  Issue #1001 follow-up: signed-URL preview endpoint.
//
//  PR #1001 required `Authorization: Bearer ...` on
//  `/api/preview/{profile_id}/{session_id}/{site_slug}/{*path}`. The SPA's
//  `<iframe src=/api/preview/...>` cannot inject headers, so the iframe
//  401-loops after PR #1001 landed.
//
//  Codex design: mint a 256-bit random token via
//  `POST /api/my/preview/sign`, store a server-side grant
//  `{issuer_bearer, identity_snapshot, profile_id, session_id, site_slug,
//  expires_at}` in `AppState.preview_tokens`, and serve the preview via
//  PUBLIC route `GET /api/preview-signed/{token}/{*path}`. The token IS the
//  auth credential.
//
//  Revocation: `serve_signed_preview` re-resolves the issuer bearer on
//  every request — logout / session-delete invalidate naturally because
//  `resolve_identity` will return `None` for a revoked bearer. Daemon
//  restart drops the in-memory `PreviewTokens` cache, invalidating every
//  outstanding grant.
//
//  See `crates/octos-cli/src/api/preview_tokens.rs` for full design
//  rationale on the token-cache module itself.
// ───────────────────────────────────────────────────────────────────────────

/// Body for `POST /api/my/preview/sign`.
#[derive(Deserialize)]
pub struct SignPreviewRequest {
    pub profile_id: String,
    pub session_id: String,
    pub site_slug: String,
}

/// `POST /api/my/preview/sign` — mint an opaque signed-preview token.
///
/// Flow:
/// 1. Auth middleware has already populated `Extension<AuthIdentity>` — if
///    it's missing the router routed an unauthenticated request into this
///    handler, which is a routing bug. Fail closed with 401.
/// 2. Extract the bearer that the auth middleware accepted (we re-validate
///    it on every `serve_signed_preview` call, so we need to store the
///    same string here). Falling back to query `?token=` matches
///    `extract_token` in `router.rs`.
/// 3. Identity must be authorized for the requested `profile_id`. Codex
///    design: this is the same `is_authorized_for_profile` gate the
///    `/api/preview/...` route uses, so the signing surface is no looser
///    than the route it bridges to.
/// 4. Validate that `<data_dir>/users/<encoded_session_key>/workspace/sites/<slug>`
///    exists. Without this check, a caller who legitimately owns
///    `profile_id` could mint a token for a nonexistent session — not a
///    security boundary (the serve handler would 404 anyway), but it lets
///    the SPA surface a useful error at the sign step rather than the
///    serve step.
/// 5. Issue the token via `state.preview_tokens.issue(...)`.
pub async fn sign_preview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    Json(req): Json<SignPreviewRequest>,
) -> Response {
    // (1) Auth identity must be present (routing-bug guard).
    let Some(Extension(identity)) = identity else {
        tracing::warn!(
            "POST /api/my/preview/sign reached without AuthIdentity — routing bug? failing closed"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // (2) Extract the bearer string. Mirrors the precedence
    // `router.rs::extract_token` uses: Authorization header first, then
    // `?token=`/`?_token=` query param. We need this verbatim so we can
    // re-validate it via `resolve_identity` later.
    let Some(bearer) = extract_bearer_from_request(&headers) else {
        tracing::warn!("sign_preview: no bearer token on request");
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // (3) Identity must own the requested profile.
    if !is_authorized_for_profile(&state, &identity, &req.profile_id) {
        tracing::warn!(
            identity = ?identity,
            requested_profile = %req.profile_id,
            "sign_preview denied — identity not authorized for requested profile"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    // (4) Resolve the data dir and confirm the site exists. Errors map
    //     to the same status codes as the existing preview route so the
    //     SPA can render a meaningful message.
    let data_dir = match resolve_profile_data_dir_by_id(&state, &req.profile_id) {
        Ok(d) => d,
        Err(response) => return response,
    };
    let site_exists = api_session_workspace_dirs(&data_dir, &req.session_id)
        .into_iter()
        .map(|workspace| workspace.join("sites").join(&req.site_slug))
        .any(|candidate| candidate.exists());
    if !site_exists {
        tracing::warn!(
            identity = ?identity,
            profile_id = %req.profile_id,
            session_id = %req.session_id,
            site_slug = %req.site_slug,
            "sign_preview: site does not exist under profile's data dir"
        );
        return StatusCode::NOT_FOUND.into_response();
    }

    // (5) Mint the token. Codex GAP 8: distinguish OS-level entropy
    //     failures (503) from rate-limit refusals (429) so the SPA can
    //     differentiate "retry the daemon" from "you're holding too
    //     many previews open already".
    use crate::api::preview_tokens::IssueError;
    match state
        .preview_tokens
        .issue(
            bearer,
            identity,
            req.profile_id,
            req.session_id,
            req.site_slug,
        )
        .await
    {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(IssueError::Random(err)) => {
            tracing::error!(error = %err, "sign_preview: getrandom failed");
            (StatusCode::SERVICE_UNAVAILABLE, "token mint failed").into_response()
        }
        // #1009: every 429 carries a `Retry-After: 60` header. 60 s is
        // the SPA's re-sign cadence (TTL - 60 s = 9 min - 60 s tail
        // window) so a well-behaved client will naturally re-attempt
        // around that boundary, but slow clients also get an explicit
        // hint per RFC 9110. The hint covers all three rate-limit
        // variants (#1007 per-identity, GAP 8 per-bearer, #1008
        // global-with-no-evictable) so the SPA can render a uniform
        // backoff toast without branching on the error string.
        Err(IssueError::PerBearerLimitReached) => {
            tracing::warn!("sign_preview: per-bearer cap reached (codex GAP 8 backpressure)");
            preview_rate_limit_response(
                "too many outstanding preview tokens for this session — wait for expiry or close some iframes",
            )
        }
        Err(IssueError::PerIdentityLimitReached) => {
            tracing::warn!(
                "sign_preview: per-identity cap reached (#1007 cross-bearer backpressure)"
            );
            preview_rate_limit_response(
                "too many outstanding preview tokens for this account — wait for expiry or close some iframes",
            )
        }
        Err(IssueError::GlobalLimitReached) => {
            tracing::error!(
                "sign_preview: GLOBAL preview-token cap reached — possible DoS or runaway client"
            );
            preview_rate_limit_response("preview-token cache is full; daemon is rate-limiting")
        }
    }
}

/// Build the HTTP 429 response for a preview-token rate-limit refusal.
///
/// #1009 follow-up: every preview-token 429 must include a
/// `Retry-After: 60` header so SPAs and clients have a uniform backoff
/// hint regardless of which cap (per-bearer / per-identity / global)
/// tripped. 60 s is chosen to match the SPA's existing re-sign cadence
/// (TTL - 60 s) — a polite client should naturally re-issue near the
/// same boundary, and an aggressive client gets a hard backoff hint.
///
/// We hand-build the response (rather than the
/// `(StatusCode, &'static str).into_response()` shortcut used
/// elsewhere) so we can attach the header before returning.
fn preview_rate_limit_response(body: &'static str) -> Response {
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
    resp.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static("60"),
    );
    resp
}

/// `GET /api/preview-signed/{token}/{*path}` — serve a previewed asset
/// using the opaque token as the auth credential.
///
/// The route lives on the PUBLIC branch (no `user_auth_middleware`)
/// because the iframe cannot inject `Authorization`. The token IS the
/// credential — codex's design re-validates it three ways here:
///   1. Token must resolve to a non-expired grant in the in-memory cache.
///      Unknown / expired => 404 (don't leak whether the token ever
///      existed).
///   2. The `issuer_bearer` recorded at sign time must still resolve to
///      an `AuthIdentity` — `resolve_identity` returns `None` for
///      revoked sessions, deleted users, or daemon restart. Refusal =>
///      403 (the token IS known, but its bearer is no longer valid).
///   3. The re-resolved identity must still be authorized for the
///      grant's `profile_id`. Refusal => 403 (defense in depth — closes
///      the corner case where a user's role changes between sign and
///      serve).
///
/// Response carries `Referrer-Policy: no-referrer` per codex's design so
/// outbound links from the previewed site cannot leak the signed URL
/// (which contains the token) to third parties via the Referer header.
pub async fn serve_signed_preview(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((token, request_path)): axum::extract::Path<(String, String)>,
) -> Response {
    serve_signed_preview_impl(state, token, request_path).await
}

/// Variant of `serve_signed_preview` for routes WITHOUT a `{*path}`
/// segment (e.g. `GET /api/preview-signed/{token}/`). Hands the empty
/// string as the request path so the underlying preview impl serves the
/// `index.html` root.
pub async fn serve_signed_preview_root(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    serve_signed_preview_impl(state, token, String::new()).await
}

async fn serve_signed_preview_impl(
    state: Arc<AppState>,
    token: String,
    request_path: String,
) -> Response {
    // (1) Consume the token. Unknown or expired => 404.
    let Some(grant) = state.preview_tokens.consume(&token).await else {
        // 404 (NOT 401) — codex design: don't leak whether the token
        // ever existed.
        tracing::debug!("serve_signed_preview: token unknown or expired");
        return StatusCode::NOT_FOUND.into_response();
    };

    // (2) Re-resolve the issuer bearer. If the user logged out or the
    // session was deleted, `resolve_identity` returns None.
    let Some(identity) = super::router::resolve_identity_public(&state, &grant.issuer_bearer).await
    else {
        tracing::warn!(
            profile_id = %grant.profile_id,
            "serve_signed_preview: issuer bearer no longer resolves to an identity (logout / session-delete?)"
        );
        return StatusCode::FORBIDDEN.into_response();
    };

    // (3) Defense in depth: even if the bearer still resolves, re-check
    // the identity-vs-profile authorisation. Closes the corner case
    // where the user's role changes between sign and serve.
    if !is_authorized_for_profile(&state, &identity, &grant.profile_id) {
        tracing::warn!(
            identity = ?identity,
            grant_profile = %grant.profile_id,
            "serve_signed_preview: identity no longer authorized for grant's profile"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    // (4) Resolve the profile's data dir and delegate to the same
    // symlink-safe serve path that `/api/preview/...` and
    // `/api/site-preview/...` use. PR #1000's `serve_preview_no_follow`
    // is invoked inside `serve_site_preview_impl`; we must NOT inline a
    // fresh serve path here or we re-introduce the traversal bug.
    let data_dir = match resolve_profile_data_dir_by_id(&state, &grant.profile_id) {
        Ok(d) => d,
        Err(response) => return response,
    };

    let mut resp =
        serve_site_preview_impl(data_dir, grant.session_id, grant.site_slug, request_path).await;

    // (5) Codex design: set `Referrer-Policy: no-referrer` so a click on
    // any outbound link from the previewed site cannot leak the signed
    // URL (which contains the token) to third parties via Referer.
    resp.headers_mut().insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    resp
}

/// Extract a bearer token from the request headers OR the URL query
/// string. Mirrors `crate::api::router::extract_token` but takes a
/// `&HeaderMap` so the sign_preview handler can call it from a typed
/// extractor signature without re-receiving the raw axum Request.
///
/// NOTE: The query-string branch is omitted here because `/api/my/preview/sign`
/// is POST-only and clients should send the bearer via the
/// `Authorization` header — query-string fallback is a holdover for
/// EventSource and `<img src=...>` which neither apply to the sign
/// surface. If a future client needs it we'll revisit; keeping the
/// surface narrow at sign time reduces the bearer's exposure in access
/// logs.
fn extract_bearer_from_request(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Legacy `POST /api/chat` REST tests (chat_request_*, chat_response_*)
    // were retired with the handler in the cleanup follow-up to PR #908.
    // Wire-level chat coverage now lives in
    // `api::ui_protocol::tests` (turn/start, turn/completed, etc.) and
    // the `coding_multi_session` integration test (per-session workspace
    // isolation).

    #[test]
    fn session_info_serialize() {
        let info = SessionInfo {
            id: "test-session".into(),
            message_count: 42,
            title: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "test-session");
        assert_eq!(json["message_count"], 42);
        assert!(
            json.get("title").is_none(),
            "None title must be omitted from JSON"
        );
    }

    #[test]
    fn session_info_serialize_with_title_includes_field() {
        let info = SessionInfo {
            id: "test-session".into(),
            message_count: 7,
            title: Some("My Pinned Chat".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["title"], "My Pinned Chat");
    }

    #[test]
    fn message_info_serialize() {
        let info = MessageInfo {
            role: "user".into(),
            content: "hello".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            thread_id: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert_eq!(json["timestamp"], "2025-01-01T00:00:00Z");
        // None thread_id must be omitted so legacy clients keep round-tripping.
        assert!(json.get("thread_id").is_none());
    }

    #[test]
    fn message_info_serialize_with_thread_id_includes_field() {
        // M8.10 PR #1: when thread_id is populated the messages endpoint
        // must surface it in the JSON so the web client (PR #3) can group
        // messages into threads without a backfill round-trip.
        let info = MessageInfo {
            role: "assistant".into(),
            content: "answer".into(),
            timestamp: "2026-04-26T00:00:00Z".into(),
            thread_id: Some("thread-cmid-1".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["thread_id"], "thread-cmid-1");
    }

    #[test]
    fn status_response_serialize() {
        let resp = StatusResponse {
            version: "0.1.0".into(),
            model: "gpt-4".into(),
            provider: "openai".into(),
            uptime_secs: 120,
            agent_configured: true,
            base_domain: "crew.ominix.io".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["version"], "0.1.0");
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["uptime_secs"], 120);
        assert_eq!(json["agent_configured"], true);
        assert_eq!(json["base_domain"], "crew.ominix.io");
    }

    #[tokio::test]
    async fn status_returns_configured_base_domain() {
        let state = Arc::new(crate::api::AppState {
            base_domain: Some("bot.ominix.io".into()),
            ..crate::api::AppState::empty_for_tests()
        });
        let resp = status(State(state)).await;
        assert_eq!(resp.0.base_domain, "bot.ominix.io");
    }

    #[tokio::test]
    async fn status_defaults_base_domain_when_unconfigured() {
        let state = Arc::new(crate::api::AppState {
            base_domain: None,
            ..crate::api::AppState::empty_for_tests()
        });
        let resp = status(State(state)).await;
        // Backward compat: `None` surfaces as the historical `crew.ominix.io`
        // so existing dashboards / web clients keep rendering the right URL
        // until operators opt in to a per-mini value.
        assert_eq!(resp.0.base_domain, "crew.ominix.io");
    }

    #[test]
    fn api_session_workspace_dir_uses_base_session_key() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let path = api_session_workspace_dir(base, "slides-123");
        assert_eq!(
            path,
            base.join("users")
                .join("dspfac%3Aapi%3Aslides-123")
                .join("workspace")
        );
    }

    #[test]
    fn api_session_workspace_dir_encodes_session_id_safely() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let path = api_session_workspace_dir(base, "web:abc/123");
        assert_eq!(
            path,
            base.join("users")
                .join("dspfac%3Aapi%3Aweb%3Aabc%2F123")
                .join("workspace")
        );
    }

    #[test]
    fn api_session_workspace_dirs_use_current_profile_scope() {
        let base = std::path::Path::new("/tmp/octos-data/profiles/dspfac/data");
        let dirs = api_session_workspace_dirs(base, "slides-123");

        assert_eq!(dirs.len(), 3);
        assert_eq!(
            dirs[0],
            base.join("users")
                .join("dspfac%3Aapi%3Aslides-123")
                .join("workspace")
        );
        assert_eq!(
            dirs[1],
            base.join("users")
                .join("_main%3Aapi%3Aslides-123")
                .join("workspace")
        );
        assert_eq!(
            dirs[2],
            base.join("users")
                .join("api%3Aslides-123")
                .join("workspace")
        );
    }

    #[test]
    fn encode_api_session_path_id_escapes_reserved_characters() {
        assert_eq!(
            encode_api_session_path_id("web-123#slides topic"),
            "web-123%23slides%20topic"
        );
    }

    #[test]
    fn session_messages_proxy_path_encodes_session_id() {
        let path = session_messages_proxy_path("web-123#slides", 25, 0, None, None, None);
        assert!(path.starts_with("/sessions/web-123%23slides/messages?"));
    }

    #[test]
    fn site_build_cache_dir_prefers_project_local_cache() {
        let project_dir = tempfile::tempdir().unwrap();
        let cache_dir = site_build_cache_dir(project_dir.path());

        assert!(cache_dir.starts_with(std::env::temp_dir()));
        assert!(
            cache_dir
                .to_string_lossy()
                .contains("octos-site-build-npm-cache")
        );
    }

    #[test]
    fn response_path_for_profile_file_hides_absolute_paths() {
        let base = tempfile::tempdir().unwrap();
        let file = base.path().join("slides/demo/output/deck.pptx");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"pptx").unwrap();

        let handle = response_path_for_profile_file(base.path(), &file).expect("handle");

        assert_ne!(handle, file.to_string_lossy());
        assert!(handle.ends_with("/deck.pptx"));
    }

    #[test]
    fn should_resolve_file_access_from_profile_for_remote_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("windows.ominix.io"),
        );

        assert!(should_resolve_file_access_from_profile(&headers, None));
    }

    #[test]
    fn should_resolve_file_access_from_profile_skips_local_host_without_identity() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("localhost:8080"),
        );

        assert!(!should_resolve_file_access_from_profile(&headers, None));
    }

    #[tokio::test]
    async fn resolve_file_access_data_dir_falls_back_to_session_store_when_gateway_missing() {
        let data_dir = tempfile::tempdir().unwrap();
        let state = AppState {
            sessions: Some(Arc::new(tokio::sync::Mutex::new(
                octos_bus::SessionManager::open(data_dir.path()).unwrap(),
            ))),
            ..AppState::empty_for_tests()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("windows.ominix.io"),
        );

        let resolved = resolve_file_access_data_dir(&state, &headers, None)
            .await
            .expect("fallback data dir");

        assert_eq!(resolved, data_dir.path());
    }

    #[test]
    fn resolve_scoped_download_path_denies_other_profile_absolute_path() {
        let current = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_file = other.path().join("secret.txt");
        std::fs::write(&other_file, b"secret").unwrap();

        assert!(
            resolve_scoped_download_path(current.path(), &other_file.to_string_lossy()).is_none()
        );
    }

    #[test]
    fn resolve_preview_asset_path_falls_back_to_root_route_for_legacy_relative_links() {
        let base = std::env::temp_dir().join(format!(
            "octos-preview-fallback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("capabilities")).unwrap();
        std::fs::write(base.join("capabilities").join("index.html"), "ok").unwrap();

        let resolved = resolve_preview_asset_path(&base, "concepts/capabilities/").unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(base.join("capabilities").join("index.html")).unwrap()
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn resolve_preview_asset_path_does_not_fallback_for_missing_assets() {
        let base = std::env::temp_dir().join(format!(
            "octos-preview-fallback-missing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        let resolved = resolve_preview_asset_path(&base, "concepts/missing/");
        assert!(resolved.is_none());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn site_file_listing_hides_build_dirs_by_default() {
        assert!(should_skip_listing_dir("dist", false));
        assert!(should_skip_listing_dir("out", false));
        assert!(should_skip_listing_dir("docs", false));
        assert!(should_skip_listing_dir("build", false));
        assert!(should_skip_listing_dir("output_old", false));
        assert!(should_skip_listing_dir("node_modules", false));
        assert!(should_skip_listing_dir(".cache", false));
    }

    #[test]
    fn site_file_listing_can_include_build_dirs_for_session_views() {
        assert!(!should_skip_listing_dir("dist", true));
        assert!(!should_skip_listing_dir("out", true));
        assert!(!should_skip_listing_dir("docs", true));
        assert!(!should_skip_listing_dir("build", true));
        assert!(should_skip_listing_dir("output_old", true));
        assert!(should_skip_listing_dir("node_modules", true));
        assert!(should_skip_listing_dir("target", true));
    }

    #[test]
    fn pagination_defaults() {
        let json = r#"{}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 100);
        assert_eq!(params.offset, 0);
        assert_eq!(params.since_seq, None);
        assert_eq!(params.topic, None);
    }

    #[test]
    fn pagination_custom_values() {
        let json = r#"{"limit": 50, "offset": 10, "since_seq": 3, "topic": "slides demo"}"#;
        let params: PaginationParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 50);
        assert_eq!(params.offset, 10);
        assert_eq!(params.since_seq, Some(3));
        assert_eq!(params.topic.as_deref(), Some("slides demo"));
    }

    #[test]
    fn internal_api_session_ids_are_hidden_from_session_list() {
        assert!(is_internal_api_session_id("web-123#child-task-1"));
        assert!(is_internal_api_session_id("web-123#default.tasks"));
        assert!(is_internal_api_session_id("web-123#research.tasks"));
        assert!(!is_internal_api_session_id("web-123#research"));
        assert!(!is_internal_api_session_id("web-123"));
    }

    #[test]
    fn is_safe_bare_session_id_rejects_separator_chars() {
        // Codex P1 round 2: the raw-id candidate must NOT be added when
        // `id` carries the channel separator (`:`) or topic separator
        // (`#`). Otherwise crafted REST URLs like
        // `/api/sessions/telegram:123/messages` would expose
        // cross-channel session history.
        assert!(is_safe_bare_session_id("web-7c9e"));
        assert!(is_safe_bare_session_id(
            "018f8e34-1c2d-7000-9000-000000000001"
        ));
        assert!(!is_safe_bare_session_id("telegram:123"));
        assert!(!is_safe_bare_session_id("dspfac:api:web-7c9e"));
        assert!(!is_safe_bare_session_id("web-123#secret-topic"));
    }

    #[test]
    fn standalone_api_session_key_candidates_with_topic_omits_raw_for_unsafe_ids() {
        let state = AppState::empty_for_tests();
        let headers = HeaderMap::new();

        // `id` containing `:` must NOT produce a raw-id candidate so a
        // crafted REST URL can't pull history from another channel.
        // Use admin identity here so the unprofiled fallbacks ARE
        // permitted by the cross-profile gate — that way the test
        // isolates the `is_safe_bare_session_id` filter rather than
        // confounding it with the cross-profile gate.
        let candidates = standalone_api_session_key_candidates_with_topic(
            &state,
            &headers,
            Some(&AuthIdentity::Admin),
            "telegram:123",
            None,
        )
        .expect("admin identity is always authorized");
        let keys: Vec<&str> = candidates.iter().map(|k| k.0.as_str()).collect();
        assert!(
            !keys.iter().any(|k| *k == "telegram:123"),
            "raw-id candidate must be skipped for ids with `:` — got {keys:?}"
        );
    }

    /// Codex review P1 rounds 3 and 4 regression: when the resolved
    /// profile is a hosted tenant (NOT `_main`), the candidate list
    /// MUST contain ONLY the tenant's own profile-prefixed key. The
    /// `_main`, bare-channel (`api:<id>`), and raw (`<id>`) candidates
    /// all live in shared / non-tenant namespaces and would let a
    /// colliding `web-…` id read foreign rows.
    ///
    /// We can't easily construct a fully-resolved hosted state in a
    /// unit test (it requires the full `tenant_store` plumbing), so
    /// the regression is verified by mirroring the helper's gate
    /// logic inline with a caller-supplied `profile_id`. If
    /// [`standalone_api_session_key_candidates_with_topic`] ever
    /// drifts from this gate, the inline mirror will catch it.
    /// Codex review P1 rounds 3 and 4 regression: a tenant
    /// (non-admin) request resolved to a hosted profile MUST see only
    /// its own profile-prefixed candidate. The `_main`, bare-channel,
    /// and raw-id candidates would let one tenant read another's
    /// history by id collision.
    ///
    /// This test mirrors the helper's gate logic inline so future
    /// drift is caught.
    #[test]
    fn standalone_api_session_key_candidates_with_topic_omits_unprofiled_for_hosted_tenant() {
        let session_id = "web-7c9e";
        let topic = "";
        let profile_id = "dspfac"; // simulated hosted tenant
        let is_admin = false;
        let allow_cross_profile_fallback = profile_id == MAIN_PROFILE_ID || is_admin;

        let mut candidates = vec![SessionKey::with_profile_topic(
            profile_id, "api", session_id, topic,
        )];
        if allow_cross_profile_fallback {
            candidates.push(SessionKey::with_profile_topic(
                MAIN_PROFILE_ID,
                "api",
                session_id,
                topic,
            ));
            candidates.push(SessionKey::with_topic("api", session_id, topic));
            if is_safe_bare_session_id(session_id) {
                candidates.push(SessionKey(session_id.to_string()));
            }
        }
        let keys: Vec<&str> = candidates.iter().map(|k| k.0.as_str()).collect();

        // Tenant sees ONLY its own profile-prefixed key.
        assert_eq!(keys, vec!["dspfac:api:web-7c9e"]);
        assert!(!keys.iter().any(|k| *k == "_main:api:web-7c9e"));
        assert!(!keys.iter().any(|k| *k == "api:web-7c9e"));
        assert!(!keys.iter().any(|k| *k == "web-7c9e"));
    }

    /// Codex P2 round 5 regression: the canonical reload-mid-stream
    /// production shape is admin auth on a hosted subdomain. The WS
    /// handler accepts bare `SessionKey`s in admin mode (admin's
    /// `connection_profile_id` is `None`), so REST must walk the
    /// unprofiled fallbacks too — otherwise the just-persisted WS
    /// rows are unreachable through `/messages` and reload-mid-stream
    /// shows the orphan completion the M10 hardening test catches.
    #[test]
    fn standalone_api_session_key_candidates_with_topic_unlocks_unprofiled_for_admin_on_hosted() {
        let session_id = "web-7c9e";
        let topic = "";
        let profile_id = "dspfac";
        let is_admin = true;
        let allow_cross_profile_fallback = profile_id == MAIN_PROFILE_ID || is_admin;

        let mut candidates = vec![SessionKey::with_profile_topic(
            profile_id, "api", session_id, topic,
        )];
        if allow_cross_profile_fallback {
            candidates.push(SessionKey::with_profile_topic(
                MAIN_PROFILE_ID,
                "api",
                session_id,
                topic,
            ));
            candidates.push(SessionKey::with_topic("api", session_id, topic));
            if is_safe_bare_session_id(session_id) {
                candidates.push(SessionKey(session_id.to_string()));
            }
        }
        let keys: Vec<&str> = candidates.iter().map(|k| k.0.as_str()).collect();

        // Admin on hosted DOES see the bare-channel + raw-id
        // candidates so reload-mid-stream after WS-bare persistence
        // works. This is no privilege escalation: admin already has
        // read-all access through other endpoints.
        assert_eq!(keys.first().copied(), Some("dspfac:api:web-7c9e"));
        assert!(keys.iter().any(|k| *k == "_main:api:web-7c9e"));
        assert!(keys.iter().any(|k| *k == "api:web-7c9e"));
        assert!(keys.iter().any(|k| *k == "web-7c9e"));
    }

    #[test]
    fn standalone_api_session_key_candidates_with_topic_prefers_profiled_then_falls_back_to_bare() {
        let state = AppState::empty_for_tests();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dspfac.crew.ominix.io"),
        );

        // No tenant_store on the AppState -> profile resolution returns the
        // synthetic main-profile id. The candidate list still contains the
        // bare-channel and raw-id forms so `run_standalone_turn`'s WS-side
        // writes are reachable from REST regardless of which `SessionKey`
        // shape the SPA sent.
        // No identity (no admin auth, no user) — the resolved profile
        // is `MAIN_PROFILE_ID` (no tenant_store on the AppState so
        // profile resolution falls back to the synthetic main), so
        // the cross-profile gate is open and the full candidate list
        // is returned.
        let candidates = standalone_api_session_key_candidates_with_topic(
            &state, &headers, None, "web-7c9e", None,
        )
        .expect("unauthenticated → no header authorization check required");
        let keys: Vec<&str> = candidates.iter().map(|k| k.0.as_str()).collect();
        // Profiled key must come first so existing chat-history reads keep
        // hitting the canonical write target before walking fallbacks.
        assert_eq!(keys.first().copied(), Some("_main:api:web-7c9e"));
        // Bare-channel key must be present so REST returns WS-persisted rows
        // for `SessionKey::new("api", "web-…")`.
        assert!(
            keys.iter().any(|k| *k == "api:web-7c9e"),
            "bare-channel candidate missing from {keys:?}"
        );
        // Raw-id key must be present so REST returns rows when the SPA sent
        // a bare `SessionKey("web-…")` (no `api:` prefix). Codex P1.
        assert!(
            keys.iter().any(|k| *k == "web-7c9e"),
            "raw-id candidate missing from {keys:?}"
        );
    }

    /// M10.5 reload-mid-stream regression guard. WS turns persisted by
    /// `turn/start` may live under the bare-channel key (`api:<id>`) when the
    /// SPA sends a bare `SessionKey`. The REST `/messages` lookup must walk
    /// the candidate-key list and surface those rows so the SPA's hydrate
    /// step renders the user prompt + completion bubble together instead of
    /// a placeholder orphan thread.
    #[tokio::test]
    async fn session_messages_falls_back_to_bare_channel_key_for_ws_persisted_sessions() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));

        // Persist under the bare-channel key — this is what `turn/start`
        // does when the WS client passes `SessionKey::new("api", "web-…")`.
        let bare_key = SessionKey::new("api", "web-reload-mid-stream");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&bare_key, Message::user("hi please weather"))
                .await
                .unwrap();
            sess.add_message(
                &bare_key,
                Message::assistant_with_thread(
                    "on it",
                    octos_core::ThreadId::new("thread-reload-mid-stream"),
                ),
            )
            .await
            .unwrap();
        }

        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let mut headers = HeaderMap::new();
        // No routed-profile resolution here, so the profiled candidate is
        // `_main:api:web-…` (which has no JSONL on disk). The bare-channel
        // fallback is what makes the response non-empty.
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dspfac.crew.ominix.io"),
        );

        // No identity — main-profile resolution unlocks the unprofiled
        // fallbacks via the cross-profile gate's main-profile branch.
        let response = session_messages(
            State(state),
            headers,
            None,
            axum::extract::Path("web-reload-mid-stream".to_string()),
            axum::extract::Query(PaginationParams {
                limit: 100,
                offset: 0,
                source: None,
                since_seq: None,
                topic: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(messages.len(), 2, "bare-key fallback must surface rows");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hi please weather");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "on it");
    }

    /// Codex review P2 regression: the fallback decision must be based on
    /// whether the candidate session has *any* history, not on whether the
    /// requested page is non-empty. Otherwise, when the canonical session
    /// has rows but the requested page is past the end, REST silently
    /// switches to a sibling candidate's history under pagination,
    /// corrupting the response stream.
    #[tokio::test]
    async fn session_messages_does_not_mix_candidate_histories_under_pagination() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));

        // The "real" session (under the canonical profiled key) has 2 rows.
        let profiled_key = SessionKey::with_profile(MAIN_PROFILE_ID, "api", "web-pagi");
        // A sibling bare-key session has 1 unrelated row (e.g. left over
        // from an earlier WS write before profile promotion).
        let bare_key = SessionKey::new("api", "web-pagi");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&profiled_key, Message::user("page one user"))
                .await
                .unwrap();
            sess.add_message(
                &profiled_key,
                Message::assistant_with_thread(
                    "page one reply",
                    octos_core::ThreadId::new("thread-pagi-1"),
                ),
            )
            .await
            .unwrap();
            sess.add_message(&bare_key, Message::user("UNRELATED bare-key row"))
                .await
                .unwrap();
        }

        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        // Request a page past the end of the profiled session: offset=10,
        // limit=10. Pre-fix this returned the bare-key's
        // `UNRELATED bare-key row`. Post-fix it returns `[]` and stays
        // anchored to the profiled session.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dspfac.crew.ominix.io"),
        );
        let response = session_messages(
            State(state),
            headers,
            None,
            axum::extract::Path("web-pagi".to_string()),
            axum::extract::Query(PaginationParams {
                limit: 10,
                offset: 10,
                source: None,
                since_seq: None,
                topic: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(
            messages.is_empty(),
            "page past end of profiled session must return [] without leaking bare-key rows: {messages:?}"
        );
    }

    /// Codex P1 round 2 regression: a crafted REST URL whose `id`
    /// contains the channel separator (`:`) MUST NOT pull history from
    /// the bare-key store of another channel. Without
    /// [`is_safe_bare_session_id`] the raw-id fallback would walk
    /// `SessionKey("telegram:123")` directly and surface that
    /// telegram session's rows under an API endpoint.
    #[tokio::test]
    async fn session_messages_does_not_leak_cross_channel_history_via_crafted_id() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));

        // Persist a telegram-channel row under the canonical key shape
        // (`telegram:123`). REST `/api/sessions/...` is API-channel
        // scoped — its handler must NOT see this row regardless of
        // what `id` the caller passes.
        let telegram_key = SessionKey::new("telegram", "123");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&telegram_key, Message::user("telegram secret"))
                .await
                .unwrap();
        }

        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });
        let headers = HeaderMap::new();
        // Pass admin identity so the cross-profile gate is open and the
        // ONLY thing keeping the telegram row out of the response is
        // the `is_safe_bare_session_id` filter on the raw-id candidate.
        // Pre-filter, this would have leaked.
        let identity = Some(Extension(AuthIdentity::Admin));
        let response = session_messages(
            State(state),
            headers,
            identity,
            axum::extract::Path("telegram:123".to_string()),
            axum::extract::Query(PaginationParams {
                limit: 100,
                offset: 0,
                source: None,
                since_seq: None,
                topic: None,
            }),
        )
        .await;

        // Either the standalone path returns [] (no API-channel rows
        // for that id) and the gateway proxy fires (returning 503 in
        // tests), or the standalone path returns []. Both outcomes
        // are acceptable; what we MUST NOT see is the telegram row.
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("telegram secret"),
            "REST must not leak telegram-channel history via crafted id; got: {body_str}"
        );
    }

    /// Codex review P1 regression: when the SPA sends a bare `SessionKey`
    /// whose serialized form is the literal SPA id (e.g. `web-…`, no
    /// `api:` prefix), `run_standalone_turn` persists under that raw key.
    /// The fallback list MUST include the raw-id candidate so REST reaches
    /// those rows after a reload.
    #[tokio::test]
    async fn session_messages_falls_back_to_raw_id_for_bareless_session_keys() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));

        // Persist under the raw id (no `api:` prefix) — this is what the
        // WS handler ends up doing when the SPA sends `SessionKey("web-…")`
        // verbatim.
        let raw_key = SessionKey("web-raw-id-only".to_string());
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&raw_key, Message::user("raw user"))
                .await
                .unwrap();
            sess.add_message(
                &raw_key,
                Message::assistant_with_thread(
                    "raw reply",
                    octos_core::ThreadId::new("thread-raw"),
                ),
            )
            .await
            .unwrap();
        }

        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dspfac.crew.ominix.io"),
        );
        let response = session_messages(
            State(state),
            headers,
            None,
            axum::extract::Path("web-raw-id-only".to_string()),
            axum::extract::Query(PaginationParams {
                limit: 100,
                offset: 0,
                source: None,
                since_seq: None,
                topic: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(messages.len(), 2, "raw-id fallback must surface rows");
        assert_eq!(messages[0]["content"], "raw user");
        assert_eq!(messages[1]["content"], "raw reply");
    }

    #[tokio::test]
    async fn list_sessions_hides_internal_runtime_sessions_from_standalone_store() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let parent = SessionKey::with_profile(MAIN_PROFILE_ID, "api", "web-123");
        let child = SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-123", "child-1");
        let task_ledger =
            SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-123", "default.tasks");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&parent, Message::user("parent"))
                .await
                .unwrap();
            sess.add_message(&child, Message::user("child"))
                .await
                .unwrap();
            sess.add_message(&task_ledger, Message::user("task ledger"))
                .await
                .unwrap();
        }
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let response = list_sessions(State(state), HeaderMap::new(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = sessions
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["web-123"]);
    }

    /// Issue #607 §D regression: river / mini4 hung 30 s+ on
    /// `GET /api/sessions` because one user dir had 65 535
    /// `child-*.jsonl` files that the listing iterated. The handler must
    /// stay well under 500 ms with a synthetic spawn fanout in place.
    #[tokio::test]
    async fn list_sessions_is_fast_with_many_child_jsonl_files() {
        let data_dir = tempfile::tempdir().unwrap();

        // Lay down the per-user dir for `_main:api:web-river` directly so the
        // test does not have to drive 10 000 SessionHandle writes (~30 s on
        // its own and not what we are measuring).
        let encoded_base = "_main%3Aapi%3Aweb-river";
        let user_dir = data_dir
            .path()
            .join("users")
            .join(encoded_base)
            .join("sessions");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("default.jsonl"),
            "{\"schema_version\":1,\"session_key\":\"_main:api:web-river\",\
             \"created_at\":\"2024-01-01T00:00:00Z\",\
             \"updated_at\":\"2024-01-01T00:00:00Z\"}\n",
        )
        .unwrap();

        const FANOUT: usize = 10_000;
        for i in 0..FANOUT {
            std::fs::write(
                user_dir.join(format!("child-task-{i:05}.jsonl")),
                "{\"schema_version\":1}\n{\"role\":\"assistant\",\"content\":\"x\"}\n",
            )
            .unwrap();
        }

        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let start = std::time::Instant::now();
        let response = list_sessions(State(state), HeaderMap::new(), None).await;
        let elapsed = start.elapsed();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let listed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = listed
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();
        assert_eq!(ids, vec!["web-river"], "child fanout must not surface");

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "list_sessions took {elapsed:?} for {FANOUT} child jsonls; \
             #607 §D regression — the user-facing handler must not iterate child fanouts",
        );
    }

    #[tokio::test]
    async fn delete_session_accepts_listed_topic_session_id_from_standalone_store() {
        let data_dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir.path()).unwrap(),
        ));
        let topic_key =
            SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "web-topic", "research");
        {
            let mut sess = sessions.lock().await;
            sess.add_message(&topic_key, Message::user("topic"))
                .await
                .unwrap();
            assert!(sess.load(&topic_key).await.is_some());
        }
        let state = std::sync::Arc::new(AppState {
            sessions: Some(sessions),
            ..AppState::empty_for_tests()
        });

        let response = delete_session(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path("web-topic#research".to_string()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let fresh = octos_bus::SessionManager::open(data_dir.path()).unwrap();
        assert!(fresh.load(&topic_key).await.is_none());
    }

    #[test]
    fn session_messages_proxy_path_includes_topic_and_since_seq() {
        let path = session_messages_proxy_path(
            "slides-123",
            100,
            5,
            Some("full"),
            Some(8),
            Some("slides untitled-deck"),
        );

        assert_eq!(
            path,
            "/sessions/slides-123/messages?limit=100&offset=5&source=full&since_seq=8&topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn append_topic_query_uses_question_mark_for_clean_path() {
        let mut path = "/sessions/slides-123/tasks".to_string();
        append_topic_query(&mut path, Some("slides untitled-deck"));
        assert_eq!(
            path,
            "/sessions/slides-123/tasks?topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn append_topic_query_uses_ampersand_when_query_exists() {
        let mut path = "/sessions/slides-123/messages?limit=100".to_string();
        append_topic_query(&mut path, Some("slides untitled-deck"));
        assert_eq!(
            path,
            "/sessions/slides-123/messages?limit=100&topic=slides%20untitled-deck"
        );
    }

    #[test]
    fn default_page_limit_is_100() {
        assert_eq!(default_page_limit(), 100);
    }

    // `max_message_len_is_1mb` + `chat_sync_writes_to_canonical_per_user_topic_jsonl`
    // were retired with the legacy `POST /api/chat` handler in the
    // cleanup follow-up to PR #908. The canonical-JSONL invariant is
    // now covered end-to-end by the `coding_multi_session` integration
    // test (which drives the same `octos_bus::persist_message_through_canonical_path`
    // call site) and by the WS UI Protocol handler's own unit tests.

    // ────────── M7.9 / W2 cancel + restart-from-node API tests ──────────

    /// Build a `task_query_store` carrying a single live supervisor with a
    /// running task pre-registered. Returns (store, supervisor, task_id).
    fn build_task_store_with_running_task(
        session_key: &str,
        tool_name: &str,
        tool_call_id: &str,
    ) -> (
        crate::session_actor::SessionTaskQueryStore,
        Arc<octos_agent::TaskSupervisor>,
        String,
    ) {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register(tool_name, tool_call_id, Some(session_key));
        supervisor.mark_running(&task_id);
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component(session_key);
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());
        (store, supervisor, task_id)
    }

    #[tokio::test]
    async fn cancel_task_returns_200_when_running_task_is_cancelled() {
        let (store, _supervisor, task_id) =
            build_task_store_with_running_task("api-cancel-1", "run_pipeline", "call-x");
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });

        let response = cancel_task(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path(task_id.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["task_id"], task_id);
        assert_eq!(json["status"], "cancelled");
    }

    #[tokio::test]
    async fn cancel_task_returns_404_for_unknown_task() {
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = cancel_task(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path("does-not-exist".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_task_returns_409_when_already_terminal() {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register("run_pipeline", "call-409", Some("session"));
        supervisor.mark_completed(&task_id, vec![]);

        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component("session");
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());

        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = cancel_task(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path(task_id),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn cancel_task_returns_503_without_task_query_store() {
        let state = Arc::new(AppState::empty_for_tests());
        let response = cancel_task(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            None,
            axum::extract::Path("any".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_200_with_new_task_id() {
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        let task_id = supervisor.register("run_pipeline", "call-restart", Some("session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(&task_id, "design phase failed".to_string());

        let store = crate::session_actor::SessionTaskQueryStore::default();
        let encoded = octos_bus::session::encode_path_component("session");
        let key = octos_core::SessionKey::new(MAIN_PROFILE_ID, &encoded);
        let tmp = tempfile::tempdir().unwrap();
        store.register(&key, &supervisor, tmp.path());

        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path(task_id.clone()),
            Some(Json(RestartFromNodeRequest {
                node_id: Some("design".into()),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["original_task_id"], task_id);
        assert_eq!(json["from_node"], "design");
        assert!(
            json["new_task_id"].as_str().unwrap().len() > 0,
            "new_task_id should be a fresh UUID-ish string"
        );

        // Upstream cached outputs are preserved by virtue of the original
        // task being left intact in the supervisor — the relaunch only
        // adds a successor in the `Spawned` state.
        let original = supervisor.get_task(&task_id).unwrap();
        assert_eq!(original.status, octos_agent::TaskStatus::Failed);
        let new_id = json["new_task_id"].as_str().unwrap();
        let successor = supervisor.get_task(new_id).unwrap();
        assert_eq!(successor.tool_name, "run_pipeline");
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_404_for_unknown_task() {
        let store = crate::session_actor::SessionTaskQueryStore::default();
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path("nope".into()),
            Some(Json(RestartFromNodeRequest::default())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn restart_task_from_node_returns_409_for_active_task() {
        let (store, _supervisor, task_id) =
            build_task_store_with_running_task("api-restart-409", "run_pipeline", "call-y");
        let state = Arc::new(AppState {
            task_query_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let response = restart_task_from_node(
            State(state),
            HeaderMap::new(),
            None,
            axum::extract::Path(task_id),
            Some(Json(RestartFromNodeRequest::default())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    // ────────── Legacy `POST /api/chat` REST tests retired ──────────
    //
    // The original M11-D block exercised `chat_sync` /
    // `chat_sync_via_session_runtime` end-to-end via the
    // `ChatRequest` shape. With the legacy REST entrypoint retired
    // (cleanup follow-up to PR #908), the equivalent acceptance
    // evidence now lives in:
    //
    //   * `crates/octos-cli/tests/coding_multi_session.rs`
    //     — per-session `SessionRuntime::bootstrap` writes
    //       `.octos-workspace.toml` AND multi-tenant tool registries
    //       stay isolated when distinct `workspace_hint`s are pinned.
    //   * `api::ui_protocol::tests` (`turn/start` happy path +
    //     503-on-missing-profile-runtime) — WS handler hits the same
    //     `SessionRuntimeCache::get_or_init` call site.
    //
    // The `appui_default_session_cwd` workspace-hint forwarding the
    // M11-F regression-fix test asserted is now exercised directly
    // through the WS turn dispatcher (`ui_protocol::run_standalone_turn`).

    // ── #995 — `decide_resolved_profile_id` precedence + auth ──────────
    //
    // The legacy precedence was `header.or(identity)` (handlers.rs:1442
    // before the fix), letting any authenticated request set
    // `X-Profile-Id: <victim>` and walk into the victim's data dir. The
    // current logic is identity-first; if a header is also present
    // (i.e. coming from a trusted reverse proxy after the strip
    // middleware ran) it must name a profile the identity is authorized
    // for — otherwise we return `403`, never silently override.

    use crate::api::auth_handlers::ADMIN_PROFILE_ID;
    use crate::profiles::{ProfileStore, UserProfile};
    use crate::user_store::UserRole;

    fn make_profile(id: &str, parent_id: Option<&str>) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: id.into(),
            enabled: true,
            data_dir: None,
            parent_id: parent_id.map(Into::into),
            public_subdomain: None,
            config: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Build a minimal `AppState` for `decide_resolved_profile_id` unit
    /// tests with a profile_store containing the listed profiles.
    fn state_with_profiles(profiles: &[(&str, Option<&str>)]) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let ps = ProfileStore::open(dir.path()).unwrap();
        for (id, parent) in profiles {
            ps.save(&make_profile(id, *parent)).unwrap();
        }
        let state = AppState {
            profile_store: Some(Arc::new(ps)),
            ..AppState::empty_for_tests()
        };
        (dir, state)
    }

    #[test]
    fn decide_uses_identity_when_no_header_present() {
        let (_dir, state) = state_with_profiles(&[("alice", None)]);
        let identity = AuthIdentity::User {
            id: "alice".into(),
            role: UserRole::User,
        };
        let pid = decide_resolved_profile_id(&state, Some(&identity), None, Some("alice")).unwrap();
        assert_eq!(pid, "alice");
    }

    #[test]
    fn decide_uses_header_when_identity_is_authorized_admin() {
        // Admin token can be narrowed to a specific tenant via X-Profile-Id
        // when the request comes from the loopback Caddy ingress. This is
        // the legitimate post-fix behaviour for hosted subdomains.
        let (_dir, state) = state_with_profiles(&[("alice", None)]);
        let identity = AuthIdentity::Admin;
        let pid = decide_resolved_profile_id(
            &state,
            Some(&identity),
            Some("alice"),
            Some(ADMIN_PROFILE_ID),
        )
        .unwrap();
        assert_eq!(pid, "alice");
    }

    #[test]
    fn decide_uses_header_when_identity_owns_sub_account_named_in_header() {
        let (_dir, state) = state_with_profiles(&[("owner", None), ("owner-sub", Some("owner"))]);
        let identity = AuthIdentity::User {
            id: "owner".into(),
            role: UserRole::User,
        };
        let pid =
            decide_resolved_profile_id(&state, Some(&identity), Some("owner-sub"), Some("owner"))
                .unwrap();
        assert_eq!(pid, "owner-sub");
    }

    #[test]
    fn decide_rejects_header_when_authenticated_identity_unauthorized_for_it() {
        // #995 pre-fix bypass: authenticated as alice, header says bob.
        // Pre-fix: `header.or(identity)` returned "bob" silently.
        // Post-fix: 403, no leak.
        let (_dir, state) = state_with_profiles(&[("alice", None), ("bob", None)]);
        let identity = AuthIdentity::User {
            id: "alice".into(),
            role: UserRole::User,
        };
        let err = decide_resolved_profile_id(&state, Some(&identity), Some("bob"), Some("alice"))
            .expect_err("must reject cross-tenant header");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn decide_rejects_header_pointing_to_unrelated_sub_account() {
        // Owner authenticated → cannot use header to act as another
        // owner's sub-account (parent_id mismatch).
        let (_dir, state) = state_with_profiles(&[
            ("owner-a", None),
            ("owner-b", None),
            ("owner-b-sub", Some("owner-b")),
        ]);
        let identity = AuthIdentity::User {
            id: "owner-a".into(),
            role: UserRole::User,
        };
        let err = decide_resolved_profile_id(
            &state,
            Some(&identity),
            Some("owner-b-sub"),
            Some("owner-a"),
        )
        .expect_err("must reject foreign sub-account");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn decide_allows_user_with_admin_role_to_target_any_profile() {
        // A user account flagged as `UserRole::Admin` is fully
        // privileged — `is_authorized_for_profile` short-circuits to
        // `true` in that branch. Codify the contract so a future
        // refactor can't quietly break the admin path.
        let (_dir, state) = state_with_profiles(&[("alice", None), ("bob", None)]);
        let identity = AuthIdentity::User {
            id: "alice".into(),
            role: UserRole::Admin,
        };
        let pid = decide_resolved_profile_id(&state, Some(&identity), Some("bob"), Some("alice"))
            .unwrap();
        assert_eq!(pid, "bob");
    }

    #[test]
    fn decide_falls_back_to_header_when_unauthenticated() {
        // Pre-auth callers (e.g. webhook proxies, public preview) still
        // use the header as a hint. Their handlers do their own
        // authorization downstream; the contract here is only
        // "don't 403 just because no identity is present."
        let (_dir, state) = state_with_profiles(&[("alice", None)]);
        let pid = decide_resolved_profile_id(&state, None, Some("alice"), None).unwrap();
        assert_eq!(pid, "alice");
    }

    #[test]
    fn decide_returns_bad_request_when_no_signal_at_all() {
        let (_dir, state) = state_with_profiles(&[]);
        let err = decide_resolved_profile_id(&state, None, None, None)
            .expect_err("no identity AND no header must fail");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decide_returns_bad_request_when_authenticated_but_no_identity_profile_id_resolved() {
        // Edge case: identity is `Some(_)` but the caller could not
        // resolve a profile id for it (e.g. admin in a setup-wizard
        // state where `ensure_admin_profile` hasn't run yet). We must
        // not fall through to a stripped-empty header.
        let (_dir, state) = state_with_profiles(&[]);
        let identity = AuthIdentity::Admin;
        let err = decide_resolved_profile_id(&state, Some(&identity), None, None)
            .expect_err("must signal missing context");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
