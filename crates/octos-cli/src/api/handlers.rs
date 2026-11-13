//! API request handlers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use octos_agent::inspect_workspace_contract;
use octos_bus::file_handle::{
    encode_profile_file_handle, encode_tmp_upload_handle, resolve_legacy_file_request,
    resolve_scoped_file_handle, resolve_workspace_file_handle,
};
// `Message` is used by non-test lib code (`dedupe_history_rows` below),
// so this import must not be test-gated.
use octos_core::Message;
use octos_core::ui_protocol::{
    RpcError, SessionFilesListParams, SessionFilesReadParams, SessionFilesReadResult, WorkMode,
    rpc_error_codes,
};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;

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
///   integration test, three e2e specs, and the former M4.1A
///   live-validation helper) migrated to the WS path.
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
    auth_profile: Option<&str>,
    session_workspace: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(resolved) = resolve_scoped_file_handle(base_dir, request_path)
        .or_else(|| resolve_legacy_file_request(base_dir, request_path))
    {
        // SECURITY (#1377 round-6 P1.2): `up/` handles + raw tmpdir refs resolve
        // against the PROCESS-GLOBAL upload root regardless of `base_dir`, so a
        // requester who obtains another tenant's handle could otherwise download
        // it. Now that uploads are stored under `octos-uploads/<profile>/…`,
        // refuse an upload-tmpdir result whose owning tenant != the authenticated
        // requester. Non-upload results (profile files already scoped to
        // `base_dir`) are unaffected.
        if let Some(owner) = upload_tmpdir_tenant(&resolved) {
            if auth_profile != Some(owner.as_str()) {
                return None;
            }
        }
        return Some(resolved);
    }
    // #1377: uploaded files are materialized into the per-session workspace
    // (`<workspace>/uploads/<name>`) and the user-message `FileRef` carries that
    // workspace-relative path. Serve it from the resolved session workspace (the
    // caller passes the SAME root the turn materialized into).
    let session_workspace = session_workspace?;
    resolve_workspace_file_handle(session_workspace, request_path)
        .or_else(|| resolve_within_workspace(session_workspace, request_path))
}

/// If `p` is contained under the process-global upload tmpdir, return its owning
/// tenant (the first path component, since uploads live at
/// `octos-uploads/<tenant>/<uuid>_<name>`). `None` for any path NOT under the
/// upload root (e.g. a profile file already scoped to its tenant `base_dir`).
fn upload_tmpdir_tenant(p: &std::path::Path) -> Option<String> {
    let root = std::fs::canonicalize(octos_bus::file_handle::temp_upload_root()).ok()?;
    let canon = std::fs::canonicalize(p).ok()?;
    match canon.strip_prefix(&root).ok()?.components().next()? {
        std::path::Component::Normal(s) => s.to_str().map(str::to_string),
        _ => None,
    }
}

/// Resolve a workspace-relative download path (e.g. `uploads/<name>`) under
/// `workspace_root`. Canonical containment + traversal guards keep it inside
/// that workspace.
fn resolve_within_workspace(
    workspace_root: &std::path::Path,
    request_path: &str,
) -> Option<std::path::PathBuf> {
    let rel = std::path::Path::new(request_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let candidate = std::fs::canonicalize(workspace_root.join(rel)).ok()?;
    let canon_ws = std::fs::canonicalize(workspace_root).ok()?;
    (candidate.is_file() && candidate.starts_with(&canon_ws)).then_some(candidate)
}

/// Resolve the workspace root for a download request's owning session, matching
/// EXACTLY where the turn materialized uploads (#1377). `data_dir` is the
/// authenticated caller's profile/tenant root, so this only reaches that
/// tenant's own sessions.
///
/// Primary: the in-memory open-session map (`session_workspace_root_for_profile`),
/// which holds the exact `workspace_root` the turn used — correct for every
/// session shape, including `base_key()`-encoded and cwd-hinted workspaces.
/// Fallback (evicted session): the canonical multi-tenant layout
/// `<data>/users/<encode_path_component(base_key)>/workspace` (mirrors the
/// workspace construction in `ui_protocol.rs`).
///
/// SECURITY: the map key includes the authenticated requester's profile id as
/// well as the session id, so a foreign profile cannot retrieve a same-named
/// SPA session. The containment check remains required: a profile can point a
/// session at an approved external cwd, but profile ownership alone is not
/// sufficient authority to serve arbitrary host files through `/api/files`.
fn resolve_session_workspace_root(
    _state: &AppState,
    data_dir: &std::path::Path,
    profile_id: Option<&str>,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let key = octos_core::SessionKey(session_id.to_string());
    // A chat's folder is its workspace, opened or not (UPCR-2026-R1EFC56A3).
    // A chat whose folder is unusable serves no files at all.
    match crate::runtime::chat_workspace::lookup(data_dir, &key) {
        Ok(Some(chat)) => return Some(chat.root),
        Ok(None) => {}
        Err(_) => return None,
    }
    if let Some(ws) =
        crate::api::ui_protocol_transport::session_workspace_root_for_profile(profile_id, &key)
    {
        if map_workspace_belongs_to_tenant(&ws, data_dir) {
            return Some(ws);
        }
    }
    let base = session_id.split('#').next().unwrap_or(session_id);
    if base.is_empty() || base.contains('/') || base.contains('\\') || base.contains("..") {
        return None;
    }
    let encoded = octos_bus::session::encode_path_component(base);
    Some(data_dir.join("users").join(encoded).join("workspace"))
}

/// Workspace binding for UPCR-2026-057. An approved external cwd recorded by
/// `session/open` is authoritative even when it is outside the profile data
/// dir; the legacy `/api/files` resolver deliberately rejects that broader
/// case and must keep its narrower behavior. The legacy fallback remains for
/// an evicted session that has no recorded binding.
fn resolve_session_workspace_root_for_read(
    state: &AppState,
    data_dir: &std::path::Path,
    profile_id: Option<&str>,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let key = octos_core::SessionKey(session_id.to_owned());
    // A chat, usable or not, never falls back to the root recorded at open.
    let not_chat = matches!(
        crate::runtime::chat_workspace::lookup(data_dir, &key),
        Ok(None)
    );
    if not_chat
        && let Some(root) =
            crate::api::ui_protocol_transport::session_workspace_root_for_profile(profile_id, &key)
    {
        return Some(root);
    }
    resolve_session_workspace_root(state, data_dir, profile_id, session_id)
}

/// True when `ws` is contained under the authenticated profile data dir.
/// Canonicalizes both sides and fails closed if either path is unavailable.
fn map_workspace_belongs_to_tenant(ws: &std::path::Path, data_dir: &std::path::Path) -> bool {
    match (std::fs::canonicalize(ws), std::fs::canonicalize(data_dir)) {
        (Ok(w), Ok(d)) => w.starts_with(&d),
        _ => false,
    }
}

/// The authenticated requester's owning profile id, derived the SAME way the WS
/// session derives its tenant (routed header OR authenticated identity), so a
/// download's tenant check matches the upload's stamp. Admin-without-routing →
/// `MAIN_PROFILE_ID` (admin runs default sessions as `_main`). `None` when it
/// can't be resolved — download callers then fail closed. Used by `/api/files`.
fn request_owner_profile(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Option<String> {
    let header_profile_id = routed_profile_id_from_headers(state, headers);
    let identity_profile_id = match identity {
        Some(AuthIdentity::User { id, .. }) => Some(id.as_str()),
        Some(AuthIdentity::Admin) => Some(ADMIN_PROFILE_ID),
        None => None,
    };
    let pid = if identity.is_none() && header_profile_id.is_none() {
        MAIN_PROFILE_ID.to_string()
    } else {
        decide_resolved_profile_id(
            state,
            identity,
            header_profile_id.as_deref(),
            identity_profile_id,
        )
        .ok()?
    };
    Some(owner_profile_for_resolved_request(
        pid,
        header_profile_id.as_deref(),
    ))
}

/// Resolve a session-scoped file request against the profile that actually
/// owns the opened session, not the requester's default profile. Unscoped
/// admin remains authorized for every profile; users stay confined to their
/// own profile and unauthenticated legacy requests remain on `_main`.
fn request_session_owner_profile(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    session_id: &str,
) -> Option<String> {
    let request_profile = request_owner_profile(state, headers, identity)?;
    let key = octos_core::SessionKey(session_id.to_owned());
    let Some(session_profile) = crate::api::ui_protocol_transport::session_bound_profile_id(&key)
    else {
        return Some(request_profile);
    };
    match identity {
        Some(AuthIdentity::Admin) => Some(session_profile),
        Some(AuthIdentity::User { .. }) if request_profile == session_profile => {
            Some(session_profile)
        }
        None if request_profile == MAIN_PROFILE_ID && session_profile == MAIN_PROFILE_ID => {
            Some(session_profile)
        }
        _ => None,
    }
}

fn session_profile_data_dir(
    state: &AppState,
    request_data_dir: &std::path::Path,
    profile_id: &str,
) -> Option<std::path::PathBuf> {
    if profile_id == MAIN_PROFILE_ID {
        Some(request_data_dir.to_path_buf())
    } else {
        resolve_profile_data_dir_by_id(state, profile_id).ok()
    }
}

fn owner_profile_for_resolved_request(
    resolved_profile_id: String,
    header_profile_id: Option<&str>,
) -> String {
    // Admin without an explicit tenant route is the legacy bootstrap/admin
    // surface and runs default sessions under `_main`. If the client explicitly
    // routes to `admin` (X-Profile-Id or hosted profile route), keep `admin` so
    // upload ownership matches the WS `session/open` runtime.
    if resolved_profile_id == ADMIN_PROFILE_ID && header_profile_id.is_none() {
        MAIN_PROFILE_ID.to_string()
    } else {
        resolved_profile_id
    }
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

pub(crate) fn encode_api_session_path_id(id: &str) -> String {
    octos_bus::session::encode_path_component(id)
}

/// Result entry shape for the WS `session/list` RPC method (formerly the
/// body of `GET /api/sessions`, retired in M12 Phase D-5).
#[derive(Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub message_count: usize,
    /// Display title (auto-derived from first user message; manual rename via
    /// the WS `session/title.set` RPC method preserves across new messages).
    /// None for legacy sessions persisted before the title field existed;
    /// the client should fall back to deriving a title from message content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Recency timestamp (RFC3339) — `SessionMeta.updated_at`, or the JSONL
    /// file mtime as a cheap fallback. Lets the TUI sort the session list by
    /// most-recently-touched. None when neither source is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Preview of the session's MOST RECENT user prompt, truncated (~100 bytes,
    /// UTF-8 safe). Sourced by scanning the tail of the session JSONL for the
    /// last user-role message (see
    /// `octos_bus::SessionManager::list_top_level_sessions_with_meta`). Lets the
    /// `/resume` picker show what each session was about. None for sessions with
    /// no user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    /// Does this session have a live (non-terminal) turn RIGHT NOW?
    ///
    /// Sourced from the process-global active-turn registry, not from disk —
    /// so it is honest about sessions the asking connection never opened,
    /// which is the point: two UI Protocol clients (the TUI and the browser
    /// client) can attach to one `octos serve`, and this is how one learns the
    /// other is mid-turn before it tries a `turn/start` that would be refused.
    ///
    /// Always serialized (unlike the `Option` fields above, where absence
    /// means "unknown"): `false` is a real, useful answer. Additive and
    /// ungated — `session/list` itself is already behind
    /// `auxiliary.rest_to_ws.v1`, and the sibling additive fields on this
    /// struct carry no capability of their own. `#[serde(default)]` keeps the
    /// gateway-merge deserialization below working against an older peer that
    /// does not emit the field.
    #[serde(default)]
    pub active_turn: bool,
    /// UPCR-2026-R6982BEFC: the mode the session's record stores. `session/list`
    /// keeps it only for connections that negotiated `session.work_mode.v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_mode: Option<WorkMode>,
}

/// Does a listing entry correspond to a session with a live turn?
///
/// The active-turn registry is keyed by the WIRE [`SessionKey`]
/// (`<profile>:api:<chat>` — what `turn/start` carries), while a per-profile
/// or per-project store lists chat-BARE ids, so reconstruct the wire key for
/// the listing's effective profile. An id that is ALREADY a full key (it
/// carries the `:` channel separator, as legacy flat-layout entries can) is
/// also matched verbatim; a chat-bare id is deliberately NOT, or profile A's
/// live turn on `A:api:web-1` would light up profile B's own `web-1` row.
/// Callers still holding the full key (the process-wide walk) match it
/// exactly instead of going through here.
pub(super) fn session_id_has_active_turn(
    active_turns: &std::collections::HashSet<SessionKey>,
    effective_profile_id: &str,
    listed_id: &str,
) -> bool {
    active_turns
        .iter()
        .any(|key| active_turn_key_matches(key, effective_profile_id, listed_id))
}

/// Does one active-turn registry key denote the session a listing row names?
///
/// The registry is keyed by the session id EXACTLY as it arrived on the wire,
/// which is whatever shape the client chose: a bare handle (`web-1`), a
/// channel-qualified key (`api:web-1`) or a fully profiled one
/// (`coding:api:web-1`). The listing stores, meanwhile, hand back either the
/// full key (the process-wide walk) or the chat id alone (per-profile and
/// per-project stores). Reconstructing a key from the row would have to guess
/// both the profile and the CHANNEL, so this inverts the mapping instead and
/// decomposes the registry key, which needs no guessing.
///
/// A profiled key matches only its own profile's rows — profile A's turn on
/// `A:api:web-1` must never light up profile B's `web-1`. An unprofiled key
/// carries no tenant dimension to check, so a bare row in another profile's
/// listing could in principle match it; that ambiguity is inherent to a
/// client that opened an unprofiled session and is not introduced here.
/// Topics are folded away by `chat_id`/`base_key`: a turn in `…#research` does
/// make the session busy.
fn active_turn_key_matches(key: &SessionKey, effective_profile_id: &str, listed_id: &str) -> bool {
    // The row carried the whole key (or the client used a colon-less handle
    // and the store listed it unchanged).
    if key.0 == listed_id {
        return true;
    }
    // A colon-less key has no chat-id component to compare — `chat_id()`
    // returns empty for it — so the verbatim check above was its only chance.
    if !key.base_key().contains(':') {
        return false;
    }
    if key.chat_id() != listed_id {
        return false;
    }
    match key.profile_id() {
        Some(profile_id) => profile_id == effective_profile_id,
        None => true,
    }
}

fn is_internal_api_session_id(id: &str) -> bool {
    id.split_once('#')
        .is_some_and(|(_, topic)| is_internal_session_topic(topic))
}

fn is_internal_session_topic(topic: &str) -> bool {
    topic.starts_with("child-") || topic == "default.tasks" || topic.ends_with(".tasks")
}

/// Typed application boundary for the read-only session surfaces (plus the
/// existing title update). The legacy response functions below remain private
/// raw gateway adapters; both REST and OUP consume the same JSON value or typed
/// error instead of OUP constructing Axum extractors and responses itself.
pub(crate) struct SessionQueryService {
    state: Arc<AppState>,
}

#[derive(Debug)]
pub(crate) struct SessionQueryErrorBody {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum SessionQueryError {
    Unauthorized(SessionQueryErrorBody),
    NotFound(SessionQueryErrorBody),
    InvalidInput(SessionQueryErrorBody),
    Conflict(SessionQueryErrorBody),
    Unavailable(SessionQueryErrorBody),
    Internal(SessionQueryErrorBody),
}

impl SessionQueryError {
    fn body(&self) -> &SessionQueryErrorBody {
        match self {
            Self::Unauthorized(body)
            | Self::NotFound(body)
            | Self::InvalidInput(body)
            | Self::Conflict(body)
            | Self::Unavailable(body)
            | Self::Internal(body) => body,
        }
    }

    fn into_body(self) -> SessionQueryErrorBody {
        match self {
            Self::Unauthorized(body)
            | Self::NotFound(body)
            | Self::InvalidInput(body)
            | Self::Conflict(body)
            | Self::Unavailable(body)
            | Self::Internal(body) => body,
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.body().status
    }

    pub(crate) fn detail(&self) -> Option<String> {
        let detail = String::from_utf8_lossy(&self.body().body).into_owned();
        if detail.is_empty() {
            None
        } else {
            Some(detail)
        }
    }

    pub(crate) fn title_detail(&self) -> Option<String> {
        String::from_utf8(self.body().body.clone()).ok()
    }

    async fn from_response(response: Response) -> Self {
        let (parts, body) = response.into_parts();
        let bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes.to_vec(),
            Err(error) => {
                return Self::Internal(SessionQueryErrorBody {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    content_type: Some("text/plain; charset=utf-8".to_owned()),
                    body: format!("session query error response body failed: {error}").into_bytes(),
                });
            }
        };
        let body = SessionQueryErrorBody {
            status: parts.status,
            content_type: parts
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body: bytes,
        };
        match parts.status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::Unauthorized(body),
            StatusCode::NOT_FOUND => Self::NotFound(body),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Self::InvalidInput(body),
            StatusCode::CONFLICT => Self::Conflict(body),
            StatusCode::SERVICE_UNAVAILABLE => Self::Unavailable(body),
            _ => Self::Internal(body),
        }
    }

    fn invalid_json(error: serde_json::Error) -> Self {
        Self::Internal(SessionQueryErrorBody {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            content_type: Some("text/plain; charset=utf-8".to_owned()),
            body: format!("invalid session query JSON response: {error}").into_bytes(),
        })
    }

    fn into_response(self) -> Response {
        let body = self.into_body();
        let mut response = (body.status, body.body).into_response();
        if let Some(content_type) = body.content_type
            && let Ok(value) = axum::http::HeaderValue::from_str(&content_type)
        {
            response
                .headers_mut()
                .insert(axum::http::header::CONTENT_TYPE, value);
        }
        response
    }
}

async fn session_query_json(response: Response) -> Result<Value, SessionQueryError> {
    if !response.status().is_success() {
        return Err(SessionQueryError::from_response(response).await);
    }
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|error| {
            SessionQueryError::Internal(SessionQueryErrorBody {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                content_type: Some("text/plain; charset=utf-8".to_owned()),
                body: format!("session query response body failed: {error}").into_bytes(),
            })
        })?;
    serde_json::from_slice(&bytes).map_err(SessionQueryError::invalid_json)
}

async fn session_query_empty(response: Response) -> Result<(), SessionQueryError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(SessionQueryError::from_response(response).await)
    }
}

fn session_query_json_response(result: Result<Value, SessionQueryError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

impl SessionQueryService {
    pub(crate) fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub(crate) async fn list(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        connection_profile_id: Option<&str>,
        cwd_sessions_root: Option<std::path::PathBuf>,
        active_turns: &std::collections::HashSet<SessionKey>,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            list_sessions_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                connection_profile_id,
                cwd_sessions_root,
                active_turns,
            )
            .await,
        )
        .await
    }

    pub(crate) async fn messages(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        id: &str,
        params: PaginationParams,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            session_messages_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                id.to_owned(),
                params,
            )
            .await,
        )
        .await
    }

    pub(crate) async fn status(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        id: &str,
        topic: Option<String>,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            session_status_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                id.to_owned(),
                topic,
            )
            .await,
        )
        .await
    }

    pub(crate) async fn tasks(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        id: &str,
        topic: Option<String>,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            session_tasks_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                id.to_owned(),
                topic,
            )
            .await,
        )
        .await
    }

    pub(crate) async fn files(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        params: &SessionFilesListParams,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            session_files_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                params.clone(),
            )
            .await,
        )
        .await
    }

    pub(crate) async fn workspace(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        id: &str,
    ) -> Result<Value, SessionQueryError> {
        session_query_json(
            session_workspace_contract_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                id.to_owned(),
            )
            .await,
        )
        .await
    }

    pub(crate) async fn title(
        &self,
        headers: &HeaderMap,
        identity: Option<&AuthIdentity>,
        id: &str,
        title: &str,
    ) -> Result<(), SessionQueryError> {
        session_query_empty(
            update_session_title_response(
                self.state.clone(),
                headers.clone(),
                identity.cloned(),
                id.to_owned(),
                title.to_owned(),
            )
            .await,
        )
        .await
    }
}

async fn list_sessions_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    connection_profile_id: Option<&str>,
    cwd_sessions_root: Option<std::path::PathBuf>,
    active_turns: &std::collections::HashSet<SessionKey>,
) -> Response {
    // Collect sessions from both the standalone store and gateway profiles.
    let mut all: Vec<SessionInfo> = Vec::new();
    let identity_ref = identity.as_ref();

    // #995 follow-up — Layer-2 authorization for the routed profile.
    // Pre-fix this prefix went straight to `routed_profile_id_from_headers`,
    // so a trusted-hop request with `X-Profile-Id: <victim>` listed the
    // victim's sessions even when authenticated as another tenant.
    let profile_id = match api_profile_id_from_headers_authorized(&state, &headers, identity_ref) {
        Ok(pid) => pid,
        Err(response) => return response,
    };

    // Effective profile scope for this listing. Resolved up here (it used to
    // sit below the cwd short-circuit) so the per-project branch can stamp
    // `active_turn` against the same profile as every other scope. Both reads
    // are pure functions of the state + headers, so hoisting them changes
    // nothing else.
    let routed_profile_id = routed_profile_id_from_headers(&state, &headers);
    // A localhost / stdio UI Protocol connection has no routed profile
    // header, but its authenticated profile is frozen onto the connection.
    // Use that scope for every legacy fallback below; defaulting to `_main`
    // here leaks solo/admin session metadata into an ordinary user's list.
    let connection_scoped_profile_id =
        connection_profile_id.filter(|_| routed_profile_id.is_none());
    let effective_profile_id = connection_scoped_profile_id.unwrap_or(&profile_id);

    // Per-project listing short-circuit (`appui.sessions_in_cwd`). The
    // authorization gate above still runs (the connection must be allowed to
    // list at all); we then scope the listing to the cwd's `<cwd>/.octos`
    // store instead of the profile/global stores. Runs BEFORE the legacy
    // merge so a project session list never bleeds in another scope's rows.
    if let Some(cwd_root) = cwd_sessions_root {
        let cwd_sessions = list_profile_sessions(&cwd_root, active_turns, effective_profile_id);
        return Json(cwd_sessions).into_response();
    }

    // M11-F per-profile SessionManager listing. Mirrors the
    // `session_messages` fix in commit 10cc9378d (`fix(api): route
    // session_messages through per-profile SessionManager`): the
    // WS turn handler (`run_standalone_turn`) writes session history
    // through `SessionRuntime.sessions` which opens against the
    // profile's own `data_dir` (`<profile>/data/sessions/...`). The
    // process-wide `state.sessions` walk below opens against the
    // top-level home (`<home>/sessions/`) and never sees those
    // per-profile JSONLs — so for an authenticated tenant request
    // (e.g. dspfac listing its own chat history on
    // `https://dspfac.octos.ominix.io/chat`), the legacy walk
    // returns 0 sessions and the chat sidebar renders empty.
    //
    // The listing reads the resolved profile_data_dir without creating a
    // store and walks the bare on-disk ids the
    // SPA actually uses — no prefix strip, because the per-profile
    // sessions/ directory is itself the tenant boundary, so on-disk
    // ids are already chat-bare. The process-wide walk below remains
    // as an admin / legacy fallback (still strips `<profile>:api:`
    // for legacy entries).
    //
    // Resolve the profile data dir. The solo/stdio connection carries no
    // HTTP routing header, so the header/identity + gateway resolver
    // (`resolve_profile_data_dir`) returns `Err` there — pre-fix the
    // per-profile `sessions/` dir was never scanned and the TUI
    // `/resume` picker showed "No prior sessions" even with sessions on
    // disk. When there is NO routing header we instead resolve from the
    // connection's own frozen profile scope (`connection_profile_id`),
    // mirroring how `session/hydrate` resolves its store for the same
    // connection (`resolve_sessions_for_lookup` keys on
    // `connection_profile_id`), so `session/list` scans the SAME
    // directory the running session writes turns to.
    //
    // SECURITY / multi-tenant: `connection_profile_id` is the
    // connection's frozen scope (authenticated identity / `session/open`),
    // never a client-supplied per-request value — so this lists the
    // connection's OWN sessions, not another tenant's. Whenever a
    // `Host` / `X-Profile-Id` routing header IS present (hosted
    // multi-tenant WS / gateway), or the connection is admin/unscoped, we
    // keep the existing header + identity authorized resolution unchanged
    // — Layer-2 authorization still applies, and a parent viewing a
    // sub-account subdomain still lists the routed profile's sessions.
    let profile_data_dir = match connection_profile_id {
        Some(pid) if routed_profile_id.is_none() => {
            resolve_profile_data_dir_by_id(&state, pid).ok()
        }
        _ => resolve_profile_data_dir(&state, &headers, identity_ref)
            .await
            .ok(),
    };
    if let Some(profile_data_dir) = profile_data_dir {
        // The profile listing reads session files synchronously; hand it to
        // the blocking pool so a cold scan parks a blocking thread instead of
        // stalling this connection's executor (#2392).
        let active_turns = active_turns.clone();
        let effective = effective_profile_id.to_string();
        let profile_sessions = match tokio::task::spawn_blocking(move || {
            list_profile_sessions(&profile_data_dir, &active_turns, &effective)
        })
        .await
        {
            Ok(profile_sessions) => profile_sessions,
            Err(err) => {
                tracing::warn!(%err, "profile session listing task failed");
                Vec::new()
            }
        };
        let existing: std::collections::HashSet<String> =
            all.iter().map(|s| s.id.clone()).collect();
        all.extend(
            profile_sessions
                .into_iter()
                .filter(|s| !existing.contains(&s.id)),
        );
    }

    if let Some(sessions) = &state.sessions {
        let sessions = sessions.clone();
        let prefix = format!("{effective_profile_id}:api:");
        // Use `list_top_level_sessions` (skips `child-*` and `*.tasks` at the
        // directory walk) so a user dir with tens of thousands of spawn
        // children does not turn this listing into an O(N) hang. The
        // `is_internal_api_session_id` belt-and-suspenders check is kept for
        // legacy entries that might slip through (e.g. flat layout files
        // pre-dating the encoder rules).
        let existing: std::collections::HashSet<String> =
            all.iter().map(|s| s.id.clone()).collect();
        // The walk reads every active session file (meta line, tail preview,
        // row count) under the manager lock; the lock is taken on the
        // blocking pool so a cold scan parks there instead of stalling an
        // async executor while it runs (#2392). `blocking_lock` is sound on
        // that pool and serializes the walk exactly as the awaited lock did.
        let listed = match tokio::task::spawn_blocking(move || {
            sessions.blocking_lock().list_top_level_sessions_with_meta()
        })
        .await
        {
            Ok(listed) => listed,
            Err(err) => {
                tracing::warn!(%err, "session listing task failed");
                Vec::new()
            }
        };
        all.extend(listed.into_iter().filter_map(
            |(id, count, title, updated_at, last_prompt, work_mode)| {
                let chat_id = id.strip_prefix(&prefix)?;
                if is_internal_api_session_id(chat_id) {
                    return None;
                }
                if existing.contains(chat_id) {
                    return None;
                }
                // The process-wide store hands us the FULL wire key
                // before the prefix strip, so match the registry exactly
                // rather than reconstructing it.
                let active_turn = active_turns.contains(&SessionKey(id.clone()));
                Some(SessionInfo {
                    id: chat_id.to_string(),
                    message_count: count,
                    title,
                    updated_at: updated_at.map(|dt| dt.to_rfc3339()),
                    last_prompt,
                    active_turn,
                    work_mode: Some(WorkMode::from_stored(work_mode.as_deref())),
                })
            },
        ));
    }

    // Also fetch from gateway if available.
    // #995 follow-up — routed_profile_id used to walk the per-profile
    // gateway is authorized above; the `resolve_api_port_authorized`
    // call re-checks header authorization belt-and-suspenders.
    let api_port = if let Some(profile_id) = connection_scoped_profile_id {
        // The frozen connection scope is authoritative on localhost. Never
        // fall back to the first running gateway, which may belong to a
        // different profile.
        match state.process_manager.as_ref() {
            Some(pm) => pm
                .api_port(profile_id)
                .await
                .map(|port| (profile_id.to_string(), port)),
            None => None,
        }
    } else {
        match resolve_api_port_authorized(&state, &headers, identity_ref).await {
            Ok(port) => port,
            Err(response) => return response,
        }
    };
    if let Some((_profile_id, port)) = api_port {
        let proxy_resp = super::webhook_proxy::api_get_proxy(&state, port, "/sessions").await;
        if proxy_resp.status().is_success() {
            if let Ok(body) = axum::body::to_bytes(proxy_resp.into_body(), 10 * 1024 * 1024).await {
                if let Ok(gateway_sessions) = serde_json::from_slice::<Vec<SessionInfo>>(&body) {
                    // Merge, dedup by id (per-profile / standalone wins).
                    let existing: std::collections::HashSet<String> =
                        all.iter().map(|s| s.id.clone()).collect();
                    all.extend(
                        gateway_sessions
                            .into_iter()
                            .filter(|s| {
                                !existing.contains(&s.id) && !is_internal_api_session_id(&s.id)
                            })
                            // The gateway is a separate process with its own
                            // registry, so trust its flag when it sets one and
                            // otherwise fall back to ours.
                            .map(|mut s| {
                                s.active_turn = s.active_turn
                                    || session_id_has_active_turn(
                                        active_turns,
                                        effective_profile_id,
                                        &s.id,
                                    );
                                s
                            }),
                    );
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

/// List top-level sessions persisted under a per-profile data_dir.
///
/// Reads top-level metadata from the resolved profile_data_dir without
/// creating a store or activating a profile in a previously unused folder.
/// The on-disk session ids are
/// already chat-bare (the SPA-supplied `web-<N>` / `slides-<N>` form),
/// so we emit them as-is without stripping a `<profile>:api:` prefix —
/// that prefix only applies to the process-wide `state.sessions` store
/// which opens against the top-level home.
///
/// Internal runtime topics (`child-*` and `*.tasks` sidecars) are
/// already filtered at the `list_top_level_sessions` directory walk;
/// the `is_internal_api_session_id` check belt-and-suspenders any
/// legacy flat-layout entries.
///
/// The per-profile sessions/ directory IS the tenant boundary — only
/// that profile's JSONLs live there — so no further authorization gate
/// is needed inside this helper. The caller (`list_sessions`) handles
/// header/identity authorization via [`resolve_profile_data_dir`]
/// before invoking us.
pub(super) fn list_profile_sessions(
    profile_data_dir: &std::path::Path,
    active_turns: &std::collections::HashSet<SessionKey>,
    effective_profile_id: &str,
) -> Vec<SessionInfo> {
    octos_bus::SessionManager::list_top_level_sessions_with_meta_at(profile_data_dir)
        .into_iter()
        .filter_map(|(id, count, title, updated_at, last_prompt, work_mode)| {
            if is_internal_api_session_id(&id) {
                return None;
            }
            let active_turn = session_id_has_active_turn(active_turns, effective_profile_id, &id);
            Some(SessionInfo {
                id,
                message_count: count,
                title,
                updated_at: updated_at.map(|dt| dt.to_rfc3339()),
                last_prompt,
                active_turn,
                work_mode: Some(WorkMode::from_stored(work_mode.as_deref())),
            })
        })
        .collect()
}

// Helper for `ui_protocol_transport::handle_session_messages_page` (M12 Phase D-5).
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

#[allow(dead_code)]
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

#[cfg_attr(not(test), allow(dead_code))]
pub async fn session_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Response {
    let result = SessionQueryService::new(state)
        .messages(&headers, identity.as_ref().map(|ext| &ext.0), &id, params)
        .await;
    session_query_json_response(result)
}

async fn session_messages_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    id: String,
    params: PaginationParams,
) -> Response {
    let limit = params.limit.min(500);
    let offset = params.offset.min(10_000);
    let identity_ref = identity.as_ref();

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

    // M11-F per-profile SessionManager lookup. The WS turn handler
    // (`run_standalone_turn`) writes session history through
    // `SessionRuntime.sessions` which opens against the profile's own
    // `data_dir` (`<profile>/data/sessions/...`). The process-wide
    // `state.sessions` below opens against the top-level home and
    // never sees profile-scoped JSONLs — so for an authenticated
    // tenant request (e.g. dspfac asking for its own slides session),
    // the legacy `state.sessions` walk returns 0 messages and the
    // left chat panel renders empty after a hard refresh. Live mini3
    // regression 2026-05-18 for session
    // `slides-1779130130502-th18yr#slides untitled-deck-th18yr`
    // (38 lines on disk under `profiles/dspfac/data/sessions/`,
    // 0 candidates matched at the process-wide layer).
    //
    // Open a transient `SessionManager` against the resolved profile's
    // data_dir and walk the bare-key candidates the SPA actually uses
    // (`<session_id>` and `<session_id>#<topic>`). The candidate
    // helper's admin/main gate is irrelevant here because the disk
    // scope IS the tenant boundary (the request's profile_data_dir
    // resolves only sessions belonging to that profile).
    if !use_full {
        if let Ok(profile_data_dir) = resolve_profile_data_dir(&state, &headers, identity_ref).await
        {
            if let Some(history) = read_profile_session_messages(
                &profile_data_dir,
                &id,
                params.topic.as_deref(),
                limit,
                offset,
            )
            .await
            {
                return Json(history).into_response();
            }
        }
    }

    // Try the process-wide standalone store next (legacy + admin fallback).
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
                        media: m.media.clone(),
                        thread_id: m.thread_id.clone(),
                        source: m.source.clone(),
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
    /// File attachments stored with this row. Kept additive and omitted when
    /// empty so legacy clients that only read text continue to see the same
    /// shape, while reload paths can restore user-uploaded image/video bubbles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    /// M8.10 PR #1 thread grouping key. Lets the web client render chat
    /// history as `Vec<Thread>` rather than a flat message list. Omitted
    /// from the JSON when `None` so legacy clients that don't read the
    /// field continue to round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Provenance tag from the persisted row (UPCR-2026-042). Present only on
    /// out-of-band records (`session/append_message`); turn-written rows
    /// omit it so legacy clients keep round-tripping cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// Helper for `ui_protocol_transport::handle_session_status_get` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/status` was retired; this
// function survives as the implementation backing the WS
// `session/status.get` RPC method.
/// Backing impl for the WS `session/status.get` RPC method.
#[allow(dead_code)]
pub async fn session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    let result = SessionQueryService::new(state)
        .status(
            &headers,
            identity.as_ref().map(|ext| &ext.0),
            &id,
            params.topic,
        )
        .await;
    session_query_json_response(result)
}

async fn session_status_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    id: String,
    topic: Option<String>,
) -> Response {
    let identity_ref = identity.as_ref();
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
        append_topic_query(&mut path, topic.as_deref());
        return super::webhook_proxy::api_get_proxy(&state, port, &path).await;
    }

    // Standalone mode — no active task tracking
    Json(serde_json::json!({
        "active": false,
    }))
    .into_response()
}

// Helper for `ui_protocol_transport::handle_session_tasks_list` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/tasks` was retired; this
// function survives as the implementation backing the WS
// `session/tasks.list` RPC method.
/// Backing impl for the WS `session/tasks.list` RPC method.
#[allow(dead_code)]
pub async fn session_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<TopicQueryParams>,
) -> Response {
    let result = SessionQueryService::new(state)
        .tasks(
            &headers,
            identity.as_ref().map(|ext| &ext.0),
            &id,
            params.topic,
        )
        .await;
    session_query_json_response(result)
}

async fn session_tasks_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    id: String,
    topic: Option<String>,
) -> Response {
    let identity_ref = identity.as_ref();
    // Proxy to gateway (task supervisor lives there).
    // #995 follow-up — authorize routed profile against identity.
    let api_port = match resolve_api_port_authorized(&state, &headers, identity_ref).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    if let Some((_profile_id, port)) = api_port {
        let encoded_id = encode_api_session_path_id(&id);
        let mut path = format!("/sessions/{encoded_id}/tasks");
        append_topic_query(&mut path, topic.as_deref());
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
        Err(octos_agent::TaskRelaunchError::MissingExecutor) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "task_executor_unavailable",
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
    /// UPCR-2026-RDA916EB5: `"file"`/`"dir"` — present only on the
    /// workspace-tree listing (cwd-bound sessions); legacy flat listings
    /// omit the field so old clients see the shape they always saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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
            kind: None,
        });
    }
}

// Helper for `ui_protocol_transport::handle_session_files_list` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/files` was retired; this
// function survives as the implementation backing the WS
// `session/files.list` RPC method.
/// Backing impl for the WS `session/files.list` RPC method.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn session_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    // Legacy REST-shaped caller: no tree params, so the response keeps the
    // pre-UPCR-2026-RDA916EB5 flat shape exactly.
    let params = SessionFilesListParams {
        session_id: id,
        prefix: None,
        depth: None,
        limit: None,
        cursor: None,
    };
    let result = SessionQueryService::new(state)
        .files(&headers, identity.as_ref().map(|ext| &ext.0), &params)
        .await;
    session_query_json_response(result)
}

/// UPCR-2026-RDA916EB5: tree-listing bounds. `depth` defaults to 1 (the
/// lazy file-tree shape: immediate children only), `limit` defaults to
/// `DEFAULT_TREE_LIMIT`; both are capped so one call stays cheap.
const FILES_TREE_MAX_DEPTH: u32 = 8;
const FILES_TREE_DEFAULT_LIMIT: usize = 500;
const FILES_TREE_MAX_LIMIT: usize = 2000;
/// Directories never expanded or listed by the tree walk: VCS internals and
/// the kernel's own session store (`.octos` holds transcripts, not user
/// files).
const FILES_TREE_EXCLUDED_NAMES: &[&str] = &[".git", ".svn", ".hg", ".octos"];

async fn session_files_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    params: SessionFilesListParams,
) -> Response {
    let identity_ref = identity.as_ref();

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

    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity_ref).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };

    // UPCR-2026-RDA916EB5: resolve the session's workspace the way
    // `files/read` does — the session-bound root is authoritative (a
    // cwd-bound web session's files live there, NOT under the legacy
    // `users/<base>/workspace` layout the fallback scans, which is why
    // the listing used to come back empty). The session-owner check
    // mirrors `files/read`: a session bound to another tenant's profile
    // is refused.
    if let Some(session_profile) =
        request_session_owner_profile(&state, &headers, identity_ref, &params.session_id)
        && let Some(workspace) = resolve_session_workspace_root_for_read(
            &state,
            &data_dir,
            Some(&session_profile),
            &params.session_id,
        )
    {
        let tree_params_supplied = params.prefix.is_some()
            || params.depth.is_some()
            || params.limit.is_some()
            || params.cursor.is_some();
        let effective = if tree_params_supplied {
            params.clone()
        } else {
            // Param-less callers keep the legacy contract: a flat, FILES-
            // ONLY listing (now from the authoritative workspace root — the
            // cwd-bound-session empty-list bug fix), bounded by the hard
            // depth/count ceilings. Directory entries are a tree-mode
            // feature and would surprise old clients.
            SessionFilesListParams {
                depth: Some(FILES_TREE_MAX_DEPTH),
                limit: Some(FILES_TREE_MAX_LIMIT as u32),
                ..params.clone()
            }
        };
        // Legacy callers expect an empty list before the workspace exists.
        if !tree_params_supplied && !workspace.exists() {
            return Json(Vec::<Value>::new()).into_response();
        }
        let listing =
            tokio::task::spawn_blocking(move || workspace_tree_listing(&workspace, &effective))
                .await;
        return match listing {
            Ok(Ok(body)) if tree_params_supplied => Json(body).into_response(),
            Ok(Ok(body)) => {
                let files: Vec<Value> = body["files"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|f| f["kind"].as_str() == Some("file"))
                    .collect();
                Json(files).into_response()
            }
            Ok(Err(error)) => workspace_file_error_json(&error),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }

    let mut files = Vec::new();
    for workspace in api_session_workspace_dirs(&data_dir, &params.session_id) {
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

fn workspace_file_error_json(error: &RpcError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": { "code": error.code, "message": error.message, "data": error.data },
        })),
    )
        .into_response()
}

/// Validate the requested `prefix` (workspace-relative, no absolute, no
/// `..`, no `\`) and canonicalize the listing root, refusing anything
/// that escapes the canonical workspace.
/// Returns `(canonical_workspace_root, canonical_listing_root)`: the
/// listing starts at the prefix dir, but every emitted path is relative
/// to the WORKSPACE root so it feeds straight back into `files/read` and
/// the pagination cursor (UPCR-2026-RDA916EB5).
fn resolve_tree_prefix(
    workspace: &std::path::Path,
    prefix: Option<&str>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), RpcError> {
    let invalid = || {
        workspace_file_error(
            rpc_error_codes::INVALID_PARAMS,
            "workspace_file_invalid_path",
            "prefix must be a workspace-relative directory path without '..'",
        )
    };
    let prefix = prefix.unwrap_or("");
    if prefix.trim().is_empty() {
        let root = std::fs::canonicalize(workspace).map_err(|_| {
            workspace_file_error(
                rpc_error_codes::RESOURCE_NOT_FOUND,
                "session_workspace_not_found",
                "session workspace was not found",
            )
        })?;
        return Ok((root.clone(), root));
    }
    let rel = std::path::Path::new(prefix);
    if rel.is_absolute()
        || prefix.contains('\\')
        || rel.components().any(|component| match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => true,
            // The walk skips these; a prefix must not reach into them either.
            std::path::Component::Normal(name) => FILES_TREE_EXCLUDED_NAMES
                .iter()
                .any(|excluded| name == std::ffi::OsStr::new(excluded)),
            std::path::Component::CurDir => false,
        })
    {
        return Err(invalid());
    }
    let canonical_root = std::fs::canonicalize(workspace).map_err(|_| {
        workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "session_workspace_not_found",
            "session workspace was not found",
        )
    })?;
    let joined = canonical_root.join(rel);
    let canonical = std::fs::canonicalize(&joined).map_err(|_| {
        workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "workspace_prefix_not_found",
            "the requested directory was not found in the workspace",
        )
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(invalid());
    }
    if !canonical.is_dir() {
        return Err(invalid());
    }
    Ok((canonical_root, canonical))
}

/// Render one workspace entry's relative path: forward-slash joined UTF-8
/// components, so distinct names (including control characters) never
/// collide. Non-UTF-8 names return `None` — the string protocol cannot
/// address them without identity ambiguity, so they are skipped.
fn workspace_relative_path(root: &std::path::Path, path: &std::path::Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        parts.push(part.to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// Bounded, deterministic workspace tree listing (UPCR-2026-RDA916EB5).
/// Entries are sorted by workspace-relative path and paginated keyset
/// (`cursor` = the last path of the previous page). Directory symlinks are
/// never traversed (no loops); file symlinks whose canonical target leaves
/// the workspace are skipped.
fn workspace_tree_listing(
    workspace: &std::path::Path,
    params: &SessionFilesListParams,
) -> Result<Value, RpcError> {
    let (root, list_root) = resolve_tree_prefix(workspace, params.prefix.as_deref())?;
    let max_depth = params.depth.unwrap_or(1).min(FILES_TREE_MAX_DEPTH);
    let limit = (params.limit.unwrap_or(FILES_TREE_DEFAULT_LIMIT as u32) as usize)
        .min(FILES_TREE_MAX_LIMIT);
    let cursor = params.cursor.as_deref().unwrap_or("");

    struct TreeEntry {
        rel: String,
        kind: &'static str,
        size: u64,
        modified: String,
    }
    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, u32)> = vec![(list_root.clone(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Deterministic order within the level, reversed so the stack pops
        // the smallest name first.
        let mut level: Vec<(
            std::ffi::OsString,
            std::path::PathBuf,
            std::fs::FileType,
            std::fs::Metadata,
        )> = Vec::new();
        for entry in read_dir.flatten() {
            // symlink_metadata semantics: `file_type()` does NOT follow
            // links, so a symlinked directory never lands on the traversal
            // stack — no loops, no out-of-workspace escape by descent.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            level.push((entry.file_name(), entry.path(), file_type, metadata));
        }
        level.sort_by(|a, b| b.0.cmp(&a.0));
        for (name, path, file_type, metadata) in level {
            let name_str = name.to_string_lossy();
            if FILES_TREE_EXCLUDED_NAMES.contains(&name_str.as_ref()) {
                continue;
            }
            if file_type.is_symlink() {
                // A symlinked file is listed only when its canonical target
                // stays inside the workspace; symlinked dirs are never
                // traversed and not listed (a "dir" kind invites expansion).
                let Ok(canonical) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canonical.starts_with(&root) || !canonical.is_file() {
                    continue;
                }
            } else if !file_type.is_dir() && !file_type.is_file() {
                continue;
            }
            let Some(rel) = workspace_relative_path(&root, &path) else {
                continue;
            };
            let is_dir = file_type.is_dir();
            // `depth` counts levels BELOW the prefix: level 0 is the
            // prefix's immediate children, so `depth: 1` (the default)
            // lists them without descending — the lazy file-tree shape.
            // Descend BEFORE the cursor filter: a directory at or before
            // the cursor can still hold post-cursor entries below it;
            // only the emitted page is keyset-filtered.
            if is_dir && depth + 1 < max_depth {
                stack.push((path, depth + 1));
            }
            if rel.as_str() <= cursor {
                continue;
            }
            entries.push(TreeEntry {
                rel: rel.clone(),
                kind: if is_dir { "dir" } else { "file" },
                size: if is_dir { 0 } else { metadata.len() },
                modified: modified_rfc3339(&metadata),
            });
        }
    }
    // The stack DFS visits a directory's children before later siblings of
    // an ancestor; the emitted page order is the lexicographic relative
    // path order, which the keyset cursor relies on.
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));

    let next_cursor = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|e| e.rel.clone())
    } else {
        None
    };
    let files: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            let filename = e
                .rel
                .rsplit('/')
                .next()
                .unwrap_or(e.rel.as_str())
                .to_string();
            serde_json::json!({
                "filename": filename,
                "path": e.rel,
                "kind": e.kind,
                "size_bytes": e.size,
                "modified_at": e.modified,
            })
        })
        .collect();
    let mut body = serde_json::json!({ "files": files });
    if let Some(cursor) = next_cursor {
        body["next_cursor"] = Value::String(cursor);
    }
    Ok(body)
}

/// UPCR-2026-057: total file-size ceiling for session workspace reads.
pub const SESSION_FILE_READ_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// UPCR-2026-057: maximum decoded bytes returned by one RPC response.
pub const SESSION_FILE_READ_CHUNK_BYTES: u64 = 512 * 1024;

fn workspace_file_error(code: i64, kind: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, message).with_data(serde_json::json!({ "kind": kind }))
}

/// Read one bounded chunk from an explicitly named file in a session workspace.
/// The caller resolves the tenant root; this helper never accepts a host path,
/// an upload handle, or a directory as a substitute for session authorization.
pub async fn session_file_read(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    params: SessionFilesReadParams,
) -> Result<SessionFilesReadResult, RpcError> {
    if let Err(_response) = authorized_routed_profile_id_from_headers(state, headers, identity) {
        return Err(workspace_file_error(
            rpc_error_codes::PERMISSION_DENIED,
            "workspace_file_access_denied",
            "session file access is not authorized",
        ));
    }
    let request_data_dir = resolve_file_access_data_dir(state, headers, identity)
        .await
        .map_err(|_response| {
            workspace_file_error(
                rpc_error_codes::PERMISSION_DENIED,
                "workspace_file_access_denied",
                "session file access is not authorized",
            )
        })?;
    let Some(session_profile) =
        request_session_owner_profile(state, headers, identity, &params.session_id)
    else {
        return Err(workspace_file_error(
            rpc_error_codes::PERMISSION_DENIED,
            "workspace_file_access_denied",
            "session file access is not authorized",
        ));
    };
    let data_dir = if session_profile == MAIN_PROFILE_ID {
        request_data_dir
    } else {
        resolve_profile_data_dir_by_id(state, &session_profile).map_err(|_response| {
            workspace_file_error(
                rpc_error_codes::PERMISSION_DENIED,
                "workspace_file_access_denied",
                "session file access is not authorized",
            )
        })?
    };
    read_session_workspace_file(state, &data_dir, Some(&session_profile), params).await
}

async fn read_session_workspace_file(
    state: &AppState,
    data_dir: &std::path::Path,
    profile_id: Option<&str>,
    params: SessionFilesReadParams,
) -> Result<SessionFilesReadResult, RpcError> {
    let relative = std::path::Path::new(&params.path);
    if params.path.trim().is_empty()
        || relative.is_absolute()
        || params.path.contains('\\')
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(workspace_file_error(
            rpc_error_codes::INVALID_PARAMS,
            "workspace_file_invalid_path",
            "path must be a workspace-relative file path without '..'",
        ));
    }
    let Some(workspace) =
        resolve_session_workspace_root_for_read(state, data_dir, profile_id, &params.session_id)
    else {
        return Err(workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "session_workspace_not_found",
            "session workspace was not found",
        ));
    };
    // Canonicalizing the approved root preserves legitimate symlinked
    // ancestors (for example macOS /var -> /private/var). Only the
    // user-controlled relative components then need the no-follow walk.
    let root = std::fs::canonicalize(&workspace).map_err(|_| {
        workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "session_workspace_not_found",
            "session workspace was not found",
        )
    })?;
    let offset = params.offset.unwrap_or(0);
    let length = params
        .length
        .map(u64::from)
        .unwrap_or(SESSION_FILE_READ_CHUNK_BYTES);
    if length == 0 {
        return Err(workspace_file_error(
            rpc_error_codes::INVALID_PARAMS,
            "workspace_file_invalid_length",
            "length must be greater than zero",
        ));
    }
    let length = length.min(SESSION_FILE_READ_CHUNK_BYTES);
    let (bytes, size_bytes, eof) =
        read_file_chunk_no_follow(root, relative, offset, length).await?;
    Ok(SessionFilesReadResult {
        session_id: params.session_id,
        path: params.path.clone(),
        size_bytes,
        offset,
        eof,
        mime_type: workspace_mime_type(relative).to_owned(),
        encoding: "base64".to_owned(),
        content_base64: BASE64_STANDARD.encode(bytes),
    })
}

fn workspace_mime_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "json" | "jsonl" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xml" => "application/xml",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

// Helper for `ui_protocol_transport::handle_session_workspace_get` (M12 Phase D-5).
// The REST route `GET /api/sessions/{id}/workspace-contract` was retired;
// this function survives as the implementation backing the WS
// `session/workspace.get` RPC method.
/// Backing impl for the WS `session/workspace.get` RPC method.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn session_workspace_contract(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let result = SessionQueryService::new(state)
        .workspace(&headers, identity.as_ref().map(|ext| &ext.0), &id)
        .await;
    session_query_json_response(result)
}

async fn session_workspace_contract_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    id: String,
) -> Response {
    let identity_ref = identity.as_ref();

    // Issue #999 — gateway-mode tenant-leak guard. Same shape as the
    // companion `session_files` handler above: pre-fix the
    // `state.sessions.is_some()` branch short-circuited to
    // `sess.data_dir()` BEFORE checking the host-routed profile, so a
    // cross-tenant header on a TRUSTED hop exposed the victim
    // profile's workspace-contract statuses. The Layer-2 gate runs
    // up-front, and data-directory selection below must remain identical
    // to the companion `session_files` surface.
    if let Err(response) = authorized_routed_profile_id_from_headers(&state, &headers, identity_ref)
    {
        return response;
    }

    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity_ref).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
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

// The error is a ready-to-return axum response. Keeping it inline avoids an
// allocation on every rejected file request and matches the profile resolvers
// below, whose handlers also return the response directly.
#[allow(clippy::result_large_err)]
async fn resolve_file_access_data_dir(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
) -> Result<std::path::PathBuf, Response> {
    // A localhost client remains profile-scoped even when this process is not
    // the multi-profile gateway. In that topology there is no profile API port,
    // so `resolve_profile_data_dir` returns 503. Falling back to the process-wide
    // SessionManager root would make session metadata visible while file APIs
    // silently scan a different directory.
    if let Some(AuthIdentity::User { id, .. }) = identity
        && state.profile_store.is_some()
    {
        let profile_id = authorized_routed_profile_id_from_headers(state, headers, identity)?
            .unwrap_or_else(|| id.clone());
        return resolve_profile_data_dir_by_id(state, &profile_id);
    }

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

/// OUP clients pass the canonical key returned by session/open or session/list.
/// Legacy REST callers pass a chat id, which still needs API-channel expansion.
/// Never send a canonical key through that expansion (or the legacy gateway's
/// default-profile fallback): it already identifies its authoritative store.
#[allow(clippy::result_large_err)]
async fn session_mutation_key_candidates(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: &str,
) -> Result<(Vec<SessionKey>, bool), Response> {
    let key = SessionKey(id.to_owned());
    if key.profile_id().is_none() && !id.starts_with("api:") {
        // A bare ID from an authenticated user without a routing header still
        // belongs to that user's profile, never the shared `_main` namespace.
        let owner = request_owner_profile(state, headers, identity).ok_or_else(|| {
            (StatusCode::FORBIDDEN, "session access is not authorized").into_response()
        })?;
        if matches!(identity, Some(AuthIdentity::User { .. })) && owner != MAIN_PROFILE_ID {
            return Ok((vec![SessionKey::with_profile(&owner, "api", id)], false));
        }
        return standalone_api_session_key_candidates_with_topic(
            state, headers, identity, id, None,
        )
        .map(|keys| (keys, false));
    }
    authorized_routed_profile_id_from_headers(state, headers, identity)?;
    if key.channel() != "api" || key.chat_id().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "expected an API session key").into_response());
    }
    // Unprofiled API keys address the shared legacy namespace, not a user's
    // implicit profile. Keep the same main/admin boundary as legacy lookup.
    if key.profile_id().is_none()
        && !matches!(identity, Some(AuthIdentity::Admin))
        && request_owner_profile(state, headers, identity).as_deref() != Some(MAIN_PROFILE_ID)
    {
        return Err((StatusCode::FORBIDDEN, "session access is not authorized").into_response());
    }
    let Some(profile_id) = request_session_owner_profile(state, headers, identity, id) else {
        return Err((StatusCode::FORBIDDEN, "session access is not authorized").into_response());
    };
    // Legacy exact API keys live in serve's top-level store. Authorization
    // above still applies, but renaming old data needs no LLM/runtime bootstrap.
    if key.profile_id().is_none() {
        return Ok((vec![key], true));
    }
    // Sidebar operations can target a session that has not been opened on this
    // connection. Bootstrap its profile before resolving the same SessionRuntime
    // used by turn persistence; do not silently fall back after bootstrap errors.
    let runtime =
        super::ui_protocol_transport::ensure_session_profile_runtime(state, Some(&profile_id))
            .await
            .map_err(|error| {
                tracing::warn!(?error, "session mutation runtime unavailable");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session runtime is unavailable",
                )
                    .into_response()
            })?;
    if runtime.is_none() && state.profile_store.is_some() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "session runtime is unavailable",
        )
            .into_response());
    }
    Ok((vec![key], true))
}

#[allow(clippy::result_large_err)]
async fn session_mutation_gateway(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: &str,
    canonical: bool,
) -> Result<Option<(String, crate::process_manager::GatewayApiEndpoint)>, Response> {
    let key = SessionKey(id.to_owned());
    let owner = if canonical {
        key.profile_id().unwrap_or(MAIN_PROFILE_ID).to_owned()
    } else {
        request_owner_profile(state, headers, identity).ok_or_else(|| {
            (StatusCode::FORBIDDEN, "session access is not authorized").into_response()
        })?
    };
    let Some(manager) = &state.process_manager else {
        return Ok(None);
    };
    // Never substitute another running profile when this owner's gateway is
    // absent. Capture its port and credential together before local mutation.
    Ok(manager
        .api_endpoint(&owner)
        .await
        .map(|endpoint| (owner, endpoint)))
}

#[allow(clippy::result_large_err)]
fn shared_session_gateway<'a>(
    gateway: Option<&'a (String, crate::process_manager::GatewayApiEndpoint)>,
    data_dir: &std::path::Path,
    key: &SessionKey,
) -> Result<Option<&'a crate::process_manager::GatewayApiEndpoint>, Response> {
    let Some((_, endpoint)) = gateway else {
        return Ok(None);
    };
    // Gateway actors only persist API keys; a raw legacy key is serve-owned.
    if key.channel() != "api" {
        return Ok(None);
    }
    let same_store = if endpoint.data_dir == data_dir {
        true
    } else {
        let canonical = || -> std::io::Result<bool> {
            Ok(endpoint.data_dir.canonicalize()? == data_dir.canonicalize()?)
        };
        canonical().map_err(|error| {
            tracing::error!(%error, "resolve gateway session store failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not resolve session store",
            )
                .into_response()
        })?
    };
    // The child's actor and mutation handler share a process-local persist lock.
    // Never rewrite its transcript from serve, even if proxying fails.
    Ok(same_store.then_some(endpoint))
}

// Helper for `ui_protocol_transport::handle_session_title_set` (M12 Phase D-5).
// The REST route `PATCH /api/sessions/{id}/title` was retired; this
// function survives as the implementation backing the WS
// `session/title.set` RPC method.
/// Backing request shape for the WS `session/title.set` RPC method
/// (formerly the body of `PATCH /api/sessions/{id}/title`). The title
/// persists across new messages — auto-derivation from the first user
/// message no longer overrides it once a manual title is set.
#[allow(dead_code)]
#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

/// Backing impl for the WS `session/title.set` RPC method.
#[allow(dead_code)]
pub async fn update_session_title(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<UpdateTitleRequest>,
) -> Response {
    match SessionQueryService::new(state)
        .title(
            &headers,
            identity.as_ref().map(|ext| &ext.0),
            &id,
            &body.title,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn update_session_title_response(
    state: Arc<AppState>,
    headers: HeaderMap,
    identity: Option<AuthIdentity>,
    id: String,
    title: String,
) -> Response {
    let title = title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    if title.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, "title must be at most 200 chars").into_response();
    }

    let mut updated = false;
    let mut delegated = false;
    let identity_ref = identity.as_ref();
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
    let (candidates, canonical) =
        match session_mutation_key_candidates(&state, &headers, identity_ref, &id).await {
            Ok(target) => target,
            Err(response) => return response,
        };
    let gateway =
        match session_mutation_gateway(&state, &headers, identity_ref, &id, canonical).await {
            Ok(gateway) => gateway,
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
        let Some(sessions) = super::ui_protocol_transport::resolve_sessions_for_mutation(
            &state,
            None,
            routed_profile_id.as_deref(),
            &key,
            canonical,
        )
        .await
        else {
            if canonical {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session store is unavailable",
                )
                    .into_response();
            }
            continue;
        };
        let mut sess = match super::session_mutation_proxy::lock_sessions(sessions).await {
            Ok(guard) => guard,
            Err(_) => return super::session_mutation_proxy::busy_response(),
        };
        match shared_session_gateway(gateway.as_ref(), &sess.data_dir(), &key) {
            Err(response) => return response,
            Ok(Some(endpoint)) => {
                delegated = true;
                let response = super::session_mutation_proxy::delegate(
                    state.clone(),
                    sess,
                    key.clone(),
                    endpoint,
                    Some(title.clone()),
                )
                .await;
                if response.status().is_success() {
                    updated = true;
                } else if response.status() != StatusCode::NOT_FOUND {
                    return response;
                }
                continue;
            }
            Ok(None) => {}
        }
        match sess.update_title_existing(&key, title.clone()).await {
            Ok(existed) => updated |= existed,
            Err(e) => {
                tracing::error!(session_key = %key, error = %e, "update_title in profile-scoped store failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save session title",
                )
                    .into_response();
            }
        }
    }

    // A legacy gateway may own the only copy, or just mirror a local session.
    // Its failure must not discard an already persisted local success.
    if let Some((profile_id, endpoint)) = gateway.filter(|_| !canonical && !delegated) {
        let path = format!("/sessions/{}/title", encode_api_session_path_id(&id));
        let body_json = serde_json::json!({ "title": title }).to_string();
        let response = super::webhook_proxy::api_patch_proxy(
            &state,
            endpoint.port,
            &path,
            body_json,
            endpoint.token.as_deref(),
        )
        .await;
        if response.status().is_success() {
            updated = true;
        } else if !updated {
            return response;
        } else {
            tracing::warn!(profile = %profile_id, status = %response.status(),
                "gateway title synchronization failed after local success");
        }
    }

    if updated {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "session not found").into_response()
    }
}

// Helper for `ui_protocol_transport::handle_session_delete` (M12 Phase D-5).
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
    let (candidates, canonical) =
        match session_mutation_key_candidates(&state, &headers, identity_ref, &id).await {
            Ok(target) => target,
            Err(response) => return response,
        };
    let gateway =
        match session_mutation_gateway(&state, &headers, identity_ref, &id, canonical).await {
            Ok(gateway) => gateway,
            Err(response) => return response,
        };
    let mut deleted = false;
    let mut delegated = false;

    // #924 BLOCK 3: route each candidate through the profile-aware
    // SessionManager resolver — turn persistence writes to the
    // profile's `SessionRuntime.sessions`, so deletes MUST hit the
    // same store. The legacy code locked only `state.sessions`, which
    // did not contain profile-scoped runtime sessions.
    for key in candidates {
        // Preserve the lookup precedence and invalidate only this profile/key.
        // Other profiles may have an independently cached copy of the same key.
        let candidate_profile_id = key
            .profile_id()
            .map(ToOwned::to_owned)
            .or_else(|| routed_profile_id.clone())
            .or_else(|| super::ui_protocol_transport::session_bound_profile_id(&key))
            .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned());
        // #2890 follow-up: the session_ref catalog entry must die with the
        // session — otherwise `session/open {session_ref}` resurrects a
        // deleted session. Scoped to THIS (profile, session id) pair, so a
        // colliding legacy id in another profile is never silently unbound.
        if let Err(error) =
            super::workspace_registry::remove_session_refs(&state, &candidate_profile_id, &key.0)
        {
            tracing::warn!(session_key = %key, error = ?error,
                "failed to remove deleted session's session_ref registry rows");
        }
        let Some(sessions) = super::ui_protocol_transport::resolve_sessions_for_mutation(
            &state,
            None,
            routed_profile_id.as_deref(),
            &key,
            canonical,
        )
        .await
        else {
            if canonical {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session store is unavailable",
                )
                    .into_response();
            }
            state
                .session_cache
                .invalidate(&candidate_profile_id, &key)
                .await;
            super::ui_protocol_transport::clear_session_side_metadata(&candidate_profile_id, &key);
            continue;
        };
        let mut sess = match super::session_mutation_proxy::lock_sessions(sessions).await {
            Ok(guard) => guard,
            Err(_) => return super::session_mutation_proxy::busy_response(),
        };
        match shared_session_gateway(gateway.as_ref(), &sess.data_dir(), &key) {
            Err(response) => return response,
            Ok(Some(endpoint)) => {
                delegated = true;
                let response = super::session_mutation_proxy::delegate(
                    state.clone(),
                    sess,
                    key.clone(),
                    endpoint,
                    None,
                )
                .await;
                if !response.status().is_success() {
                    return response;
                }
                deleted = true;
                state
                    .session_cache
                    .invalidate(&candidate_profile_id, &key)
                    .await;
                super::ui_protocol_transport::clear_session_side_metadata(
                    &candidate_profile_id,
                    &key,
                );
                continue; // The worker also cleared the sidecar before replying.
            }
            Ok(None) => {}
        }
        let existed = match sess.clear_existing(&key).await {
            Ok(existed) => existed,
            Err(e) => {
                tracing::error!(session_key = %key, error = %e, "delete session from profile-scoped store failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not delete session",
                )
                    .into_response();
            }
        };
        // `SessionManager::clear` removes only the chat JSONL. The
        // per-session reasoning/thinking effort lives in a sidecar
        // (`<topic>.reasoning_effort.json`) next to it, keyed off the same
        // `data_dir`. Clear it for every candidate INDEPENDENTLY of whether a
        // chat JSONL loaded: a sidecar can outlive its JSONL — an older delete
        // that left it behind, or a `session/open` that wrote the sidecar for
        // an otherwise-fresh key before any message was persisted. Without this
        // unlink a deleted session's stale effort is resurrected on the next
        // `session/open` of the same key. The helper reuses
        // `reasoning_effort_path` so the encoding matches, and is
        // idempotent (missing file = `Ok(())`).
        let data_dir = sess.data_dir();
        // A chat's workspace is deleted with it (UPCR-2026-R1EFC56A3).
        super::ui_protocol_transport::remove_session_chat_workspace(&state, &key);
        if let Err(e) = super::ui_protocol_reasoning_effort::clear_reasoning_effort(&data_dir, &key)
        {
            tracing::warn!(
                session_key = %key,
                error = %e,
                "failed to clear per-session reasoning_effort sidecar on delete"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not delete session settings",
            )
                .into_response();
        }
        deleted |= existed;
        // Cache hits take the manager lock when refreshing transcript weight.
        // Release it before acquiring the cache's write lock.
        drop(sess);
        state
            .session_cache
            .invalidate(&candidate_profile_id, &key)
            .await;
        super::ui_protocol_transport::clear_session_side_metadata(&candidate_profile_id, &key);
    }

    if let Some((profile_id, endpoint)) = gateway.filter(|_| !canonical && !delegated) {
        let path = format!("/sessions/{}", encode_api_session_path_id(&id));
        let response = super::webhook_proxy::api_delete_proxy(
            &state,
            endpoint.port,
            &path,
            endpoint.token.as_deref(),
        )
        .await;
        if !response.status().is_success() {
            if !deleted {
                return response;
            }
            tracing::warn!(profile = %profile_id, status = %response.status(),
                "gateway deletion synchronization failed after local success");
        }
    }

    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/upload -- upload files, returns paths for use as turn-input media handles.
///
/// Accepts multipart/form-data with one or more `file` fields.
/// Returns JSON array of server-side upload handles.
pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    // #1377 tenant isolation: stamp the upload with the OWNING profile so the
    // turn-start materializer can verify ownership (`octos-uploads/<profile>/…`).
    // The id MUST equal the session's `tenant_id` (both `profile.id`), so it is
    // resolved IDENTITY-AWARE exactly like the session's profile — NOT from the
    // routing header alone: an OTP/bearer user on the bare host has no routed
    // header but their session is scoped to their own profile, so a header-only
    // `_main` fallback would mis-stamp the upload and make it unreadable (codex
    // round-5 P1). Cross-tenant `X-Profile-Id` → 403; truly unauthenticated
    // (no identity, no header) → `MAIN_PROFILE_ID` (the default profile's tenant).
    let identity_ref = identity.as_ref().map(|ext| &ext.0);
    let header_profile_id = routed_profile_id_from_headers(&state, &headers);
    let identity_profile_id = match identity_ref {
        Some(AuthIdentity::User { id, .. }) => Some(id.as_str()),
        Some(AuthIdentity::Admin) => Some(ADMIN_PROFILE_ID),
        None => None,
    };
    let resolved_profile_id = if identity_ref.is_none() && header_profile_id.is_none() {
        MAIN_PROFILE_ID.to_string()
    } else {
        decide_resolved_profile_id(
            &state,
            identity_ref,
            header_profile_id.as_deref(),
            identity_profile_id,
        )
        .map_err(|_| (StatusCode::FORBIDDEN, "forbidden".to_string()))?
    };
    // Keep upload ownership aligned with the WS session runtime. Admin without
    // an explicit route remains `_main`; explicit `X-Profile-Id: admin` stays
    // `admin`, otherwise voice turns in the admin Home profile lose their audio
    // during tenant materialization.
    let profile_id =
        owner_profile_for_resolved_request(resolved_profile_id, header_profile_id.as_deref());

    // Determine upload directory (per-tenant subdir).
    let upload_dir = std::env::temp_dir().join("octos-uploads").join(&profile_id);
    // #2817: private (0700) creation + foreign-owner refusal, mirroring
    // #2805's media-dir hardening; the root is hardened first so a planted
    // root is refused before anything is created inside it.
    octos_bus::media::ensure_private_dir(&octos_bus::file_handle::temp_upload_root())
        .and_then(|()| octos_bus::media::ensure_private_dir(&upload_dir))
        .map_err(|e| {
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

#[derive(Deserialize)]
pub struct FileMutationRequest {
    path: String,
    session: Option<String>,
    #[serde(flatten)]
    operation: FileMutation,
}

#[derive(Deserialize)]
pub struct SlideEditQuery {
    session_id: String,
    slug: String,
}

async fn slide_edit_location(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    query: &SlideEditQuery,
) -> Result<(std::path::PathBuf, std::path::PathBuf), Response> {
    if query.session_id.is_empty()
        || query.session_id.len() > 512
        || super::file_mutations::validate_filename(&query.slug).is_err()
    {
        return Err((StatusCode::BAD_REQUEST, "Invalid slides project.").into_response());
    }
    let data_dir = resolve_file_access_data_dir(state, headers, identity).await?;
    let root = data_dir
        .canonicalize()
        .map_err(|_| StatusCode::NOT_FOUND.into_response())?;
    let project = api_session_workspace_dirs(&root, &query.session_id)
        .into_iter()
        .map(|workspace| workspace.join("slides").join(&query.slug))
        .find(|path| path.is_dir())
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Slides scaffold not found.").into_response())?;
    let relative = project
        .strip_prefix(&root)
        .map_err(|_| StatusCode::FORBIDDEN.into_response())?
        .join(super::slide_edits::FILENAME);
    Ok((root, relative))
}

fn slide_edit_error(error: std::io::Error) -> Response {
    let status = match error.kind() {
        std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        std::io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let message = if status == StatusCode::CONFLICT {
        "Another editor saved a newer revision. Reload before saving."
    } else {
        "Unable to access slide edits. Check the project and retry."
    };
    (status, message).into_response()
}

pub async fn get_slide_edits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(query): axum::extract::Query<SlideEditQuery>,
) -> Response {
    let (root, relative) = match slide_edit_location(
        &state,
        &headers,
        identity.as_ref().map(|ext| &ext.0),
        &query,
    )
    .await
    {
        Ok(location) => location,
        Err(response) => return response,
    };
    match tokio::task::spawn_blocking(move || super::slide_edits::read(&root, &relative)).await {
        Ok(Ok(document)) => Json(document).into_response(),
        Ok(Err(error)) => slide_edit_error(error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn save_slide_edits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Query(query): axum::extract::Query<SlideEditQuery>,
    Json(request): Json<super::slide_edits::SaveRequest>,
) -> Response {
    let (root, relative) = match slide_edit_location(
        &state,
        &headers,
        identity.as_ref().map(|ext| &ext.0),
        &query,
    )
    .await
    {
        Ok(location) => location,
        Err(response) => return response,
    };
    match tokio::task::spawn_blocking(move || super::slide_edits::save(&root, &relative, request))
        .await
    {
        Ok(Ok(document)) => Json(document).into_response(),
        Ok(Err(error)) => slide_edit_error(error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum FileMutation {
    Delete,
    Rename { filename: String },
}

pub async fn mutate_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    Json(request): Json<FileMutationRequest>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let profile = request_owner_profile(&state, &headers, identity);
    let session_profile = request
        .session
        .as_deref()
        .and_then(|session| request_session_owner_profile(&state, &headers, identity, session));
    let session_data_dir = session_profile
        .as_deref()
        .and_then(|session_profile| session_profile_data_dir(&state, &data_dir, session_profile));
    let workspace = request.session.as_deref().and_then(|session| {
        resolve_session_workspace_root(
            &state,
            session_data_dir.as_ref()?,
            session_profile.as_deref(),
            session,
        )
    });
    let Some(path) = resolve_scoped_download_path(
        &data_dir,
        &request.path,
        profile.as_deref(),
        workspace.as_deref(),
    ) else {
        return (StatusCode::NOT_FOUND, "File not found in this profile.").into_response();
    };
    let root = if let Some(owner) = upload_tmpdir_tenant(&path) {
        octos_bus::file_handle::temp_upload_root().join(owner)
    } else {
        data_dir.clone()
    };
    let filename = match request.operation {
        FileMutation::Delete => None,
        FileMutation::Rename { filename } => Some(filename),
    };
    let result = tokio::task::spawn_blocking(move || {
        super::file_mutations::mutate(&root, &path, filename.as_deref())
    })
    .await;
    match result {
        Ok(Ok(path)) => Json(serde_json::json!({
            "path": path.as_deref().and_then(|path| response_path_for_profile_file(&data_dir, path)),
            "filename": path.as_deref().and_then(|path| path.file_name()).map(|name| name.to_string_lossy().to_string()),
        })).into_response(),
        Ok(Err(err)) => {
            let status = match err.kind() {
                std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
                std::io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, "Unable to change file. Check its name and permissions.").into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "File mutation failed.").into_response(),
    }
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
    // #1377: the authenticated requester's profile gates upload-tmpdir downloads
    // (no cross-tenant) and authorizes a session-relative `uploads/<name>` path.
    let auth_profile = request_owner_profile(&state, &headers, identity);
    let session_profile = params
        .get("session")
        .and_then(|session| request_session_owner_profile(&state, &headers, identity, session));
    let session_data_dir = session_profile
        .as_deref()
        .and_then(|session_profile| session_profile_data_dir(&state, &data_dir, session_profile));
    let session_ws = params.get("session").and_then(|s| {
        resolve_session_workspace_root(
            &state,
            session_data_dir.as_ref()?,
            session_profile.as_deref(),
            s,
        )
    });
    let delivery_profile = if params.contains_key("session") {
        session_profile.as_deref()
    } else {
        auth_profile.as_deref()
    };
    let delivered = delivered_copy_for_request(&state, delivery_profile, &params, filename);
    serve_file_impl(
        &data_dir,
        filename,
        auth_profile.as_deref(),
        session_ws.as_deref(),
        delivered,
    )
    .await
}

/// The stored copy of a file the agent delivered in the request's `session`,
/// looked up under the session-owning profile data dir. That profile is the
/// authorization boundary; the session id selects a copy within it.
struct DeliveredDownloadRequest {
    profile_root: std::path::PathBuf,
    session: SessionKey,
    reference: String,
    retention: octos_bus::session_artifacts::ArtifactRetentionPolicy,
}

fn delivered_copy_for_request(
    state: &AppState,
    auth_profile: Option<&str>,
    params: &std::collections::HashMap<String, String>,
    filename: &str,
) -> Option<DeliveredDownloadRequest> {
    let session_id = params.get("session")?;
    let profile = auth_profile?;
    let profile = if profile.is_empty() {
        MAIN_PROFILE_ID
    } else {
        profile
    };
    let profile_root = resolve_profile_data_dir_by_id(state, profile).ok()?;
    let retention = state
        .profiles
        .get(profile)
        .map(|runtime| runtime.artifact_retention)
        .or_else(|| {
            state.profile_store.as_ref().and_then(|store| {
                store
                    .get(profile)
                    .ok()
                    .flatten()
                    .and_then(|profile| profile.config.artifact_retention)
            })
        })
        .unwrap_or_default();
    Some(DeliveredDownloadRequest {
        profile_root,
        session: SessionKey(session_id.clone()),
        reference: filename.to_string(),
        retention,
    })
}

/// GET /api/files/:filename -- serve uploaded files and pipeline report files.
pub async fn serve_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    identity: Option<Extension<AuthIdentity>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
        Ok(data_dir) => data_dir,
        Err(response) => return response,
    };
    let auth_profile = request_owner_profile(&state, &headers, identity);
    let session_profile = params
        .get("session")
        .and_then(|session| request_session_owner_profile(&state, &headers, identity, session));
    let session_data_dir = session_profile
        .as_deref()
        .and_then(|session_profile| session_profile_data_dir(&state, &data_dir, session_profile));
    let session_ws = params.get("session").and_then(|s| {
        resolve_session_workspace_root(
            &state,
            session_data_dir.as_ref()?,
            session_profile.as_deref(),
            s,
        )
    });
    let delivery_profile = if params.contains_key("session") {
        session_profile.as_deref()
    } else {
        auth_profile.as_deref()
    };
    let delivered = delivered_copy_for_request(&state, delivery_profile, &params, &filename);
    serve_file_impl(
        &data_dir,
        &filename,
        auth_profile.as_deref(),
        session_ws.as_deref(),
        delivered,
    )
    .await
}

/// Read a file's bytes through an `O_NOFOLLOW` open (Unix) so a symlink leaf
/// swapped in after a path was validated cannot redirect the read. On
/// non-Unix, re-checks `symlink_metadata` before reading (best-effort, still
/// racy but matches the platform's capabilities). The blocking I/O runs on a
/// blocking thread.
async fn read_file_no_follow(path: std::path::PathBuf) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        {
            if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
        }
        let mut file = opts.open(&path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

#[cfg(unix)]
fn open_workspace_file(
    root: &std::path::Path,
    relative: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    // Walk the approved root itself component-by-component. NOFOLLOW on only
    // the final leaf would still allow a post-validation ancestor swap.
    let mut directory = open("/", directory_flags, Mode::empty())?;
    for component in root.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                directory = openat(&directory, name, directory_flags, Mode::empty())?;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "invalid workspace root",
                ));
            }
        }
    }
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "invalid workspace file path",
            ));
        };
        if components.peek().is_some() {
            directory = openat(&directory, name, directory_flags, Mode::empty())?;
        } else {
            return Ok(
                openat(&directory, name, file_flags, Mode::empty()).map(std::fs::File::from)?
            );
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "empty workspace file path",
    ))
}

#[cfg(not(unix))]
fn open_workspace_file(
    root: &std::path::Path,
    relative: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    let path = super::file_mutations::checked_path(root, relative)?;
    let metadata = path.symlink_metadata()?;
    if metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "workspace path is not a regular file",
        ));
    }
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "workspace path is not a regular file",
        ));
    }
    std::fs::OpenOptions::new().read(true).open(path)
}

fn workspace_open_error(error: std::io::Error) -> RpcError {
    match error.kind() {
        #[cfg(unix)]
        _ if error.raw_os_error() == Some(libc::ELOOP)
            || error.kind() == std::io::ErrorKind::NotADirectory =>
        {
            workspace_file_error(
                rpc_error_codes::PERMISSION_DENIED,
                "workspace_file_escape",
                "workspace file must stay inside the session workspace",
            )
        }
        #[cfg(not(unix))]
        _ if error.kind() == std::io::ErrorKind::NotADirectory => workspace_file_error(
            rpc_error_codes::PERMISSION_DENIED,
            "workspace_file_escape",
            "workspace file must stay inside the session workspace",
        ),
        std::io::ErrorKind::PermissionDenied => workspace_file_error(
            rpc_error_codes::PERMISSION_DENIED,
            "workspace_file_escape",
            "workspace file must stay inside the session workspace",
        ),
        std::io::ErrorKind::NotFound => workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "workspace_file_not_found",
            "workspace file was not found",
        ),
        std::io::ErrorKind::IsADirectory => workspace_file_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "workspace_file_not_regular",
            "workspace path is not a regular file",
        ),
        _ => workspace_file_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_file_read_failed",
            "workspace file could not be read",
        ),
    }
}

async fn read_file_chunk_no_follow(
    root: std::path::PathBuf,
    relative: &std::path::Path,
    offset: u64,
    length: u64,
) -> Result<(Vec<u8>, u64, bool), RpcError> {
    let relative = relative.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek};

        let mut file = open_workspace_file(&root, &relative).map_err(workspace_open_error)?;
        let size = file.metadata().map_err(workspace_open_error)?.len();
        if size > SESSION_FILE_READ_MAX_BYTES {
            return Err(workspace_file_error(
                rpc_error_codes::INVALID_PARAMS,
                "workspace_file_too_large",
                format!("workspace file exceeds the {SESSION_FILE_READ_MAX_BYTES}-byte limit"),
            ));
        }
        if offset > size {
            return Err(workspace_file_error(
                rpc_error_codes::INVALID_PARAMS,
                "workspace_file_offset_out_of_range",
                "offset exceeds the workspace file size",
            ));
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(workspace_open_error)?;
        let mut bytes = Vec::new();
        file.take(length)
            .read_to_end(&mut bytes)
            .map_err(workspace_open_error)?;
        let eof = offset.saturating_add(bytes.len() as u64) >= size;
        Ok((bytes, size, eof))
    })
    .await;
    match result {
        Ok(value) => value,
        Err(_) => Err(workspace_file_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_file_read_failed",
            "workspace file could not be read",
        )),
    }
}

async fn serve_file_impl(
    data_dir: &std::path::Path,
    filename: &str,
    auth_profile: Option<&str>,
    session_workspace: Option<&std::path::Path>,
    delivered_copy: Option<DeliveredDownloadRequest>,
) -> Response {
    let resolved =
        resolve_scoped_download_path(data_dir, filename, auth_profile, session_workspace);
    let (path, data) = if let Some(path) = resolved {
        // Keep the existing nofollow read for ordinary profile/workspace files.
        let data = match read_file_no_follow(path.clone()).await {
            Ok(data) => data,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        (path, data)
    } else if let Some(delivered) = delivered_copy {
        let profile_root = delivered.profile_root;
        let session = delivered.session;
        let reference = delivered.reference;
        let retention = delivered.retention;
        let opened = tokio::task::spawn_blocking(move || {
            octos_bus::session_artifacts::open_delivered_download(
                &profile_root,
                &session,
                &reference,
                retention,
            )
        })
        .await;
        let (path, file) = match opened {
            Ok(Ok(octos_bus::session_artifacts::DeliveredDownloadOpen::Present { path, file })) => {
                (path, file)
            }
            Ok(Ok(octos_bus::session_artifacts::DeliveredDownloadOpen::Missing)) => {
                return (StatusCode::FORBIDDEN, "access denied").into_response();
            }
            Ok(Ok(octos_bus::session_artifacts::DeliveredDownloadOpen::Evicted)) => {
                return (
                    StatusCode::GONE,
                    Json(serde_json::json!({
                        "error": {
                            "code": "artifact_cleaned",
                            "message": "artifact was cleaned by retention"
                        }
                    })),
                )
                    .into_response();
            }
            Ok(Err(_)) | Err(_) => {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        // The profile lock has been released; read ONLY this already-open fd.
        // A racing eviction/replacement can unlink or replace the name but
        // cannot change the bytes returned for this accepted download.
        let mut file = tokio::fs::File::from_std(file);
        let mut data = Vec::new();
        if file.read_to_end(&mut data).await.is_err() {
            return StatusCode::NOT_FOUND.into_response();
        }
        (path, data)
    } else {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
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

    let display_name: String = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string())
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '"' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();

    let content_disposition =
        match format!("inline; filename=\"{display_name}\"").parse::<axum::http::HeaderValue>() {
            Ok(value) => value,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", content_type.parse().unwrap());
    // SVG can execute as a top-level document even when used as an image
    // elsewhere. Isolate it at the response boundary, including delivered
    // copies, without imposing document sandbox restrictions on PDF viewers.
    if content_type == "image/svg+xml" {
        headers.insert(
            "content-security-policy",
            axum::http::HeaderValue::from_static(
                "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:",
            ),
        );
    }
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    headers.insert("content-disposition", content_disposition);

    (StatusCode::OK, headers, data).into_response()
}

/// Open a fresh `SessionManager` against the resolved profile's data dir
/// and try the SPA-bare key candidates the WS turn handler actually
/// writes under. Returns the requested message page if a candidate has
/// non-empty history; `None` otherwise (caller falls back to the
/// process-wide `state.sessions` block).
///
/// The bare-key candidate (`<id>` and `<id>#<topic>`) is tried
/// unconditionally inside the per-profile scope because the on-disk
/// scope IS the tenant boundary — `profile_data_dir/sessions/` only
/// contains that profile's JSONLs. The admin / main-profile gating in
/// [`standalone_api_session_key_candidates_with_topic`] is for the
/// process-wide `state.sessions`, where bare keys could theoretically
/// match a JSONL written by a different tenant.
/// Refs #2102: collapse exact double-persisted rows (NEW-16 re-entry class)
/// before they are mapped for a page payload. Adjacent byte-identical rows
/// (same role, thread_id, content, timestamp) are by construction double
/// appends — `Message` rows carry no explicit seq field, so tuple identity
/// within a contiguous run is the durable row identity available here.
fn dedupe_history_rows(history: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    for m in history {
        if let Some(last) = out.last() {
            if last.role == m.role
                && last.thread_id == m.thread_id
                && last.content == m.content
                && last.timestamp == m.timestamp
            {
                continue;
            }
        }
        out.push(m.clone());
    }
    out
}

async fn read_profile_session_messages(
    profile_data_dir: &std::path::Path,
    session_id: &str,
    topic: Option<&str>,
    limit: usize,
    offset: usize,
) -> Option<Vec<MessageInfo>> {
    let fetch_count = offset.checked_add(limit)?;
    let topic = topic.unwrap_or_default();
    let mut mgr = octos_bus::SessionManager::open(profile_data_dir).ok()?;
    let mut candidates: Vec<SessionKey> = Vec::with_capacity(2);
    if is_safe_bare_session_id(session_id) {
        if topic.is_empty() {
            candidates.push(SessionKey(session_id.to_string()));
        } else {
            candidates.push(SessionKey(format!("{session_id}#{topic}")));
            candidates.push(SessionKey(session_id.to_string()));
        }
    }
    for key in &candidates {
        let session = mgr.get_or_create(key).await;
        if session.get_history(1).is_empty() {
            continue;
        }
        // Refs #2102: collapse exact double-persisted rows (NEW-16 re-entry
        // class) before the `MessageInfo` mapping. `Message` rows carry no
        // explicit seq (seq is append-order derived), so byte-identical
        // consecutive (role, thread_id, content, timestamp) tuples inside one
        // history window are by construction double-appends. Adjacency is
        // required so legitimately repeated user text in different turns is
        // never collapsed.
        let deduped = dedupe_history_rows(session.get_history(fetch_count));
        let messages: Vec<MessageInfo> = deduped
            .iter()
            .skip(offset)
            .take(limit)
            .map(|m| MessageInfo {
                role: m.role.to_string(),
                content: m.content.clone(),
                timestamp: m.timestamp.to_rfc3339(),
                media: m.media.clone(),
                thread_id: m.thread_id.clone(),
                source: m.source.clone(),
            })
            .collect();
        return Some(messages);
    }
    None
}

/// Reject bare `session_id`s that could escape the `users/<id>/workspace`
/// subtree once joined as a raw path component.
///
/// The other candidates in [`api_session_workspace_dirs`] percent-encode
/// the id via [`octos_bus::session::encode_path_component`], so traversal
/// bytes (`/`, `\`, `..`, NUL, control chars) are neutralized. The
/// SPA-bare candidate, however, joins the id verbatim so it lines up with
/// what SPA writers (`slides-…`, `web-…`, `site-…`) put on disk — which
/// means a request-supplied id like `../escape`, `/abs`, or `foo/bar`
/// would land outside the intended profile subtree.
///
/// We allow only the alphanumeric + `-` + `_` + `#` alphabet that the SPA
/// actually uses (`#` separates the optional topic suffix). Anything else
/// — including `/`, `\`, `:`, `.`, NUL, and any non-ASCII byte — causes
/// the bare candidate to be skipped. The encoded candidates still cover
/// the request, so callers don't lose any legitimate lookup capability;
/// they just don't get the unsafe raw-join shortcut.
///
/// Codex P1 follow-up to #1069.
fn is_bare_path_safe_session_id(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    // Reject `.` / `..` outright — even surrounded by safe chars, a literal
    // path component of `..` is unambiguous traversal once joined.
    if session_id == "." || session_id == ".." {
        return false;
    }
    session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'#')
}

fn api_session_workspace_dirs(
    data_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let profile_id = infer_profile_id_from_data_dir(data_dir);
    let mut dirs = Vec::with_capacity(4);
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

    // SPA-created sessions (slides-/web-/site- session_ids) write their
    // workspace under the BARE session_id, without the `<channel>:` prefix
    // that gets percent-encoded by `encode_path_component`. The three
    // channel-prefixed encodings above don't match those on-disk paths,
    // so the listing endpoint returned an empty set and the SlidesChat
    // scaffold-wait loop timed out with "slides scaffold did not appear"
    // (live mini3 regression 2026-05-18 for session
    // `slides-1779125959003-ugtbaa`, even though the scaffold itself
    // succeeded server-side and the 3 expected files were on disk).
    //
    // SECURITY: the bare candidate joins `session_id` verbatim, so a
    // request-supplied id containing `/`, `\`, `..`, or NUL would escape
    // the intended `users/<id>/workspace` subtree (codex P1 review of
    // #1069). Gate this candidate behind a strict allowlist; the other
    // three (percent-encoded) candidates still cover any caller, they
    // just don't pick up the SPA-bare on-disk shape.
    if is_bare_path_safe_session_id(session_id) {
        let bare_path = data_dir.join("users").join(session_id).join("workspace");
        if seen.insert(bare_path.clone()) {
            dirs.push(bare_path);
        }
    }

    dirs
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

// The error is a fully-built axum response which callers return directly.
// Boxing would add allocation and dereference churn without reducing retained
// response state; use the same targeted exception as the by-id resolver below.
#[allow(clippy::result_large_err)]
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

// The `Err` variant is a fully-built axum `Response` (large by design);
// handlers return it directly for `?`-propagation, so boxing would only
// churn every call site on a cold error path.
#[allow(clippy::result_large_err)]
pub(crate) fn resolve_profile_data_dir_by_id(
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
            // Generated HTML must keep an opaque origin even when opened
            // directly in a new tab. An iframe attribute alone cannot protect
            // the application's localStorage or parent/opener documents.
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "sandbox allow-scripts allow-forms",
            ),
            (axum::http::header::REFERRER_POLICY, "no-referrer"),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
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
    let data_dir = match resolve_file_access_data_dir(&state, &headers, identity).await {
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

// Helper for `ui_protocol_transport::handle_system_status_get` (M12 Phase D-5).
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
    // Only this signed capability route is public. Opaque preview documents
    // send Origin: null for ES modules/fonts; allow those asset reads without
    // relaxing the authenticated API's exact-origin CORS policy.
    resp.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
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
#[path = "session_mutation_tests.rs"]
mod session_mutation_tests;

#[cfg(test)]
#[path = "session_file_security_tests.rs"]
mod session_file_security_tests;
