//! `octos serve --host-managed`: a loopback server owned by an embedding host.
//!
//! An app shell (for example an OctoSense phone or desktop shell) runs ONE
//! `octos serve` for its own native clients and may let the person attach an
//! external client (a web client or a terminal UI) to the same agent runtime.
//! Loopback is not an authentication boundary on a phone (any installed app
//! can connect to 127.0.0.1) or on a multi-user computer, so this mode keeps
//! the bearer token mandatory and distinguishes two credentials:
//!
//! | Credential | Source | Identity | Reaches |
//! | --- | --- | --- | --- |
//! | host token | stdin, first line | [`AuthIdentity::Admin`] | every route, as a normal admin token |
//! | external token | stdin, second line (empty: none) | `User { _main, role: User }` | `/api/ui-protocol/ws`, an allowlist of methods |
//!
//! Neither token is written anywhere, returned by any route (except the
//! external token, once, to a successful pairing claim the host enabled), or
//! logged. They never enter the environment (see [`HOST_TOKEN_ENV`]).
//!
//! What else the mode changes (see `docs/HOST_MANAGED_SERVE.md`):
//!
//! - no solo login, no trusted-proxy `X-Profile-Id`, no hashed admin-token
//!   store, no `OCTOS_TEST_TOKEN`, no OTP sessions: only the two tokens;
//! - the `Host` header must name the bound loopback listener, which blocks
//!   DNS rebinding;
//! - the browser `Origin` allowlist is only the configured origins;
//! - an external identity cannot answer approvals or questions of the
//!   host-owned app-peer sessions (`peer-…`, `peerctx-…`);
//! - an approval or question raised by an external client's turn reaches
//!   only that client, and only that client answers it (never the host);
//! - pairing is off until the host asks for a code;
//! - `server/shutdown` is never offered: the host owns the lifecycle, and the
//!   process stops when its stdin reaches EOF (the host exited or closed it).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use octos_core::{MAIN_PROFILE_ID, SessionKey};

use super::AppState;
use super::pairing::PairingState;
use super::router::{AuthIdentity, constant_time_eq};
use crate::user_store::UserRole;

/// The host token's usual env var, REFUSED in this mode: the host writes its
/// tokens on stdin ([`read_tokens_from_stdin`]). The environment is readable
/// by every process of this user through `/proc/<pid>/environ`, argv by
/// every local process, and a config file outlives the host.
pub const HOST_TOKEN_ENV: &str = "OCTOS_AUTH_TOKEN";

/// Also refused in this mode, for the same reason.
pub const EXTERNAL_TOKEN_ENV: &str = "OCTOS_HOST_EXTERNAL_TOKEN";

/// Tokens shorter than this are refused (128 bits of hex).
pub const MIN_TOKEN_LEN: usize = 32;

/// The only route an external identity may use.
pub const EXTERNAL_ROUTE: &str = "/api/ui-protocol/ws";

/// `data.kind` of a refused external answer to a host-owned peer.
pub const HOST_OWNED_PEER_ANSWER_DENIED: &str = "host_owned_peer_answer_denied";

/// `data.kind` of an external call outside [`EXTERNAL_ALLOWED_METHODS`].
pub const EXTERNAL_METHOD_DENIED: &str = "external_method_denied";

/// `data.kind` of an external call that names a host-owned app peer's
/// session (`peer-…`, `peerctx-…`).
pub const HOST_OWNED_PEER_SESSION_DENIED: &str = "host_owned_peer_session_denied";

/// `data.kind` of an external call naming a profile other than `_main`.
pub const EXTERNAL_PROFILE_DENIED: &str = "external_profile_denied";

/// `data.kind` of an external call carrying a parameter it may not set
/// (sandbox overrides, non-upload media).
pub const EXTERNAL_PARAMETER_DENIED: &str = "external_parameter_denied";

/// `data.kind` of an external steer, interrupt or answer for a turn the
/// connection did not start.
pub const EXTERNAL_TURN_DENIED: &str = "external_turn_denied";

/// The refusal for an external answer or turn control on a host turn.
pub fn external_turn_denied(what: &str) -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(format!(
        "an external client may answer or steer only its own turns ({what})"
    ))
    .with_data(serde_json::json!({ "kind": EXTERNAL_TURN_DENIED }))
}

/// `data.kind` of an `approval/respond` from any connection but the external
/// client whose turn raised the approval (the host included).
pub const EXTERNAL_APPROVAL_OWNER_ONLY: &str = "external_approval_owner_only";

/// The refusal for an answer to an external client's approval from any other
/// connection.
pub fn external_approval_owner_only() -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(
        "this approval was raised by an external client's turn; only that client answers it",
    )
    .with_data(serde_json::json!({ "kind": EXTERNAL_APPROVAL_OWNER_ONLY }))
}

/// `data.kind` of a `user_question/respond` from any connection but the
/// external client whose turn asked the question (the host included).
pub const EXTERNAL_QUESTION_OWNER_ONLY: &str = "external_question_owner_only";

/// The refusal for an answer to an external client's question from any other
/// connection.
pub fn external_question_owner_only() -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(
        "this question was asked by an external client's turn; only that client answers it",
    )
    .with_data(serde_json::json!({ "kind": EXTERNAL_QUESTION_OWNER_ONLY }))
}

/// Prompts (approvals and questions) raised by an external connection's
/// turn: approval or question id (both UUIDs) → that connection. In memory,
/// like the host-routed approvals of UPCR-2026-035.
static EXTERNAL_PROMPTS: std::sync::LazyLock<Mutex<crate::peers::host_tools::BoundedMap<u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(crate::peers::host_tools::BoundedMap::new()));

/// Record that the approval or question `prompt_id` was raised by a turn of
/// the external connection `connection`: only that connection sees it (live,
/// on replay, in pending lists and hydrate) or answers it. Registered before
/// the prompt reaches the ledger.
pub(crate) fn register_external_prompt(prompt_id: &str, connection: u64) {
    EXTERNAL_PROMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(prompt_id.to_owned(), connection);
}

/// The external connection whose turn raised `prompt_id`, if any.
pub(crate) fn external_prompt_owner(prompt_id: &str) -> Option<u64> {
    EXTERNAL_PROMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(prompt_id)
        .copied()
}

/// Forget `prompt_id`'s owner, as an eviction would (tests only).
#[cfg(test)]
pub(crate) fn forget_external_prompt(prompt_id: &str) {
    EXTERNAL_PROMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(prompt_id);
}

/// Whether `connection` may see or answer the approval or question
/// `prompt_id` under the external-client rule: only its owner for an
/// external client's prompt, anyone (subject to the other rules) otherwise.
pub(crate) fn external_prompt_visible(prompt_id: &str, connection: u64) -> bool {
    external_prompt_owner(prompt_id).is_none_or(|owner| owner == connection)
}

/// `data.kind` of an external answer on a session it did not open.
pub const EXTERNAL_SESSION_NOT_OPENED: &str = "external_session_not_opened";

/// The only methods an external connection may call (UPCR-2026-036); every
/// other method, typed or raw, is refused. Sessions and turns of non-peer
/// sessions, message paging, answers to prompts on sessions this connection
/// opened, and read-only capability and status queries.
pub const EXTERNAL_ALLOWED_METHODS: &[&str] = &[
    "config/capabilities/list",
    "session/status/read",
    "system/status.get",
    "session/open",
    "session/hydrate",
    "session/messages_page",
    "session/status.get",
    "turn/start",
    "turn/interrupt",
    "turn/steer",
    "turn/state/get",
    "approval/respond",
    "approval/scopes/list",
    "user_question/respond",
    "diff/preview/get",
];

/// Methods that answer a prompt: allowed only on a session this external
/// connection opened.
const EXTERNAL_ANSWER_METHODS: &[&str] = &["approval/respond", "user_question/respond"];

/// Every string under a key containing `session`, at any depth.
fn session_ids(value: &serde_json::Value, under_session_key: bool, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if under_session_key => out.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                session_ids(item, under_session_key, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                // Once under a `*session*` key, every nested string counts
                // (an object- or array-shaped key cannot reset the scan).
                session_ids(
                    item,
                    under_session_key || key.to_ascii_lowercase().contains("session"),
                    out,
                );
            }
        }
        _ => {}
    }
}

/// Every string under a key containing `topic`, at any depth.
fn topics(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => items.iter().for_each(|item| topics(item, out)),
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                match item.as_str() {
                    Some(topic) if key.to_ascii_lowercase().contains("topic") => {
                        out.push(topic.to_owned())
                    }
                    _ => topics(item, out),
                }
            }
        }
        _ => {}
    }
}

fn is_peer_topic(topic: &str) -> bool {
    let topic = topic.trim().to_ascii_lowercase();
    topic.starts_with("peer-")
        || topic.starts_with(crate::peers::app_binding::PEER_CONTEXT_TOPIC_PREFIX)
}

/// A host-owned app-peer session named anywhere: a `*session*` value whose
/// topic (or any `#`-suffix, in any case) is a peer's, or a `*topic*` value
/// (`session/open` and `turn/start` take the topic separately).
fn names_peer_session(params: &serde_json::Value) -> bool {
    let mut ids = Vec::new();
    session_ids(params, false, &mut ids);
    let mut named_topics = Vec::new();
    topics(params, &mut named_topics);
    ids.iter().any(|id| {
        is_peer_session(&SessionKey(id.clone())) || id.split('#').skip(1).any(is_peer_topic)
    }) || named_topics.iter().any(|topic| is_peer_topic(topic))
}

/// Parameters an external client may not set: sandbox overrides (they widen
/// read access or turn the sandbox off) and turn media that are not upload
/// handles (a raw path would pass through to the model).
fn sets_forbidden_parameter(params: &serde_json::Value) -> bool {
    fn bad_media(item: &serde_json::Value) -> bool {
        item.as_array().is_some_and(|media| {
            media.iter().any(|entry| {
                let path = entry
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .or(entry.as_str())
                    .unwrap_or("");
                !path.starts_with("up/") || path.contains("..")
            })
        })
    }
    fn walk(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Array(items) => items.iter().any(walk),
            serde_json::Value::Object(map) => map.iter().any(|(key, item)| {
                let key = key.to_ascii_lowercase();
                // No sandbox override, no chosen workspace (`cwd`), and no
                // separate `topic`: the topic is folded into the session key
                // by the handlers, so it could name an app peer's session.
                ((key.contains("sandbox") || key == "cwd" || key.contains("topic"))
                    && !item.is_null())
                    || (key == "media" && bad_media(item))
                    || walk(item)
            }),
            _ => false,
        }
    }
    walk(params)
}

/// A `profile_id` (at any depth) or a session's profile other than `_main`:
/// the external identity is the `_main` profile and nothing else.
fn names_other_profile(params: &serde_json::Value) -> bool {
    fn profiles(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Array(items) => items.iter().for_each(|item| profiles(item, out)),
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    match item.as_str() {
                        Some(profile) if key.contains("profile") => out.push(profile.to_owned()),
                        _ => profiles(item, out),
                    }
                }
            }
            _ => {}
        }
    }
    let mut named = Vec::new();
    profiles(params, &mut named);
    let mut ids = Vec::new();
    session_ids(params, false, &mut ids);
    named.extend(
        ids.iter()
            .filter_map(|id| SessionKey(id.clone()).profile_id().map(ToOwned::to_owned)),
    );
    named.iter().any(|profile| profile != MAIN_PROFILE_ID)
}

/// Decide an external connection's call: the allowlist, no host-owned peer
/// session in any `*session*` parameter, and answers only on sessions the
/// connection opened.
pub fn external_gate(
    method: &str,
    params: &serde_json::Value,
    opened_sessions: &std::collections::HashSet<String>,
) -> Result<(), octos_core::ui_protocol::RpcError> {
    use octos_core::ui_protocol::RpcError;
    if !EXTERNAL_ALLOWED_METHODS.contains(&method) {
        return Err(RpcError::permission_denied(format!(
            "{method} is not available to external clients of a host-managed server"
        ))
        .with_data(serde_json::json!({ "kind": EXTERNAL_METHOD_DENIED })));
    }
    if sets_forbidden_parameter(params) {
        return Err(RpcError::permission_denied(format!(
            "{method}: external clients may not set a sandbox, cwd or topic, or attach local files"
        ))
        .with_data(serde_json::json!({ "kind": EXTERNAL_PARAMETER_DENIED })));
    }
    if names_other_profile(params) {
        return Err(RpcError::permission_denied(format!(
            "{method}: external clients reach the {MAIN_PROFILE_ID} profile only"
        ))
        .with_data(serde_json::json!({ "kind": EXTERNAL_PROFILE_DENIED })));
    }
    if names_peer_session(params) {
        if EXTERNAL_ANSWER_METHODS.contains(&method) {
            return Err(peer_answer_denied(if method == "approval/respond" {
                "approval"
            } else {
                "question"
            }));
        }
        return Err(RpcError::permission_denied(format!(
            "{method}: host-owned app peer sessions belong to the host"
        ))
        .with_data(serde_json::json!({ "kind": HOST_OWNED_PEER_SESSION_DENIED })));
    }
    // Turn control and answers only for this connection's own turns (the
    // shared system conversation also runs the host's turns) are decided by
    // the handlers against the owner the server recorded, never against a
    // client-chosen turn id: `turn/interrupt`, `turn/steer`,
    // `approval/respond` and `user_question/respond`.
    if EXTERNAL_ANSWER_METHODS.contains(&method) {
        let session = params.get("session_id").and_then(serde_json::Value::as_str);
        if !session.is_some_and(|session| opened_sessions.contains(session)) {
            return Err(RpcError::permission_denied(format!(
                "{method}: open the session on this connection first"
            ))
            .with_data(serde_json::json!({ "kind": EXTERNAL_SESSION_NOT_OPENED })));
        }
    }
    Ok(())
}

/// Tools a turn started by an external client keeps: a fixed set of
/// built-in tools that read and edit the session workspace, search and fetch
/// the web, ask the person, and recall memory. Default-deny: code and
/// command execution, delegation, administration, peers, MCP server and
/// plugin tools (whatever their names) are all absent.
pub const EXTERNAL_TURN_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "diff_edit",
    "apply_patch",
    "glob",
    "grep",
    "list_dir",
    "code_structure",
    "check_workspace_contract",
    "web_search",
    "web_fetch",
    "ask_user_question",
    "recall",
    "recall_memory",
    "memory_search",
    "memory_load",
    "view_image",
    "view_video",
    "tool_search",
];

/// Whether an external turn keeps the tool `name` ([`EXTERNAL_TURN_TOOLS`]).
pub fn external_turn_tool_allowed(name: &str) -> bool {
    EXTERNAL_TURN_TOOLS.contains(&name)
}

/// Confine an external turn's finished registry: only compiled-in tools
/// ([`octos_agent::ToolOrigin::Builtin`]) named in [`EXTERNAL_TURN_TOOLS`]
/// survive. The names alone would not do: a plugin or MCP server can offer a
/// tool under an allowlisted name.
pub fn confine_external_turn_tools(registry: &mut octos_agent::ToolRegistry) {
    registry.retain_builtin(external_turn_tool_allowed);
}

/// Host-managed authentication and lifecycle state (`AppState::host_managed`).
pub struct HostManaged {
    host_token: String,
    external_token: Option<String>,
    /// Lower-case `Host` values that name this listener.
    allowed_hosts: Vec<String>,
    server_origin: String,
    /// The pairing code the host asked for, if any. Pairing is off (the
    /// `/pair/*` routes answer 404) while this is `None`.
    pairing: Mutex<Option<Arc<PairingState>>>,
}

impl std::fmt::Debug for HostManaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostManaged")
            .field("external_access", &self.external_token.is_some())
            .field("allowed_hosts", &self.allowed_hosts)
            .finish_non_exhaustive()
    }
}

fn validate_token(name: &str, token: &str) -> eyre::Result<()> {
    eyre::ensure!(
        token.len() >= MIN_TOKEN_LEN,
        "{name} must be at least {MIN_TOKEN_LEN} characters for --host-managed"
    );
    // Header- and subprotocol-safe: RFC 7230 `tchar`s only, so the token can
    // ride in `Authorization`, in `Sec-WebSocket-Protocol` and in a query.
    eyre::ensure!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
        "{name} may contain only letters, digits and !#$%&'*+-.^_`|~"
    );
    Ok(())
}

impl HostManaged {
    /// Validate both credentials for a listener on `127.0.0.1:<port>`.
    pub fn new(
        host_token: String,
        external_token: Option<String>,
        port: u16,
    ) -> eyre::Result<Self> {
        validate_token(HOST_TOKEN_ENV, &host_token)?;
        let external_token = external_token.filter(|token| !token.is_empty());
        if let Some(external) = &external_token {
            validate_token(EXTERNAL_TOKEN_ENV, external)?;
            eyre::ensure!(
                !constant_time_eq(external.as_bytes(), host_token.as_bytes()),
                "{EXTERNAL_TOKEN_ENV} must differ from {HOST_TOKEN_ENV}"
            );
        }
        eyre::ensure!(port != 0, "--host-managed needs the bound port");
        Ok(Self {
            host_token,
            external_token,
            allowed_hosts: vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ],
            server_origin: format!("http://127.0.0.1:{port}"),
            pairing: Mutex::new(None),
        })
    }

    /// Resolve a bearer token. Only the two configured tokens authenticate.
    pub fn resolve(&self, token: &str) -> Option<AuthIdentity> {
        if token.is_empty() {
            return None;
        }
        if constant_time_eq(token.as_bytes(), self.host_token.as_bytes()) {
            return Some(AuthIdentity::Admin);
        }
        match &self.external_token {
            Some(external) if constant_time_eq(token.as_bytes(), external.as_bytes()) => {
                Some(external_identity())
            }
            _ => None,
        }
    }

    /// Whether the request's `Host` names this listener.
    pub fn host_allowed(&self, headers: &HeaderMap, authority: Option<&str>) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .or(authority);
        host.is_some_and(|host| {
            let host = host.trim().to_ascii_lowercase();
            self.allowed_hosts.contains(&host)
        })
    }

    /// The pairing state the host enabled, if any.
    pub fn pairing(&self) -> Option<Arc<PairingState>> {
        self.pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Mint a fresh single-use, five-minute code for the external token. A
    /// previous code is replaced. `None` when external access is disabled.
    pub fn enable_pairing(&self) -> Option<Arc<PairingState>> {
        let token = self.external_token.clone()?;
        // Rate-limited, not burned: a local process guessing cannot lock out
        // the person's code (10 failures a minute; 32^8 codes).
        let pairing = Arc::new(PairingState::mint_rate_limited(
            self.server_origin.clone(),
            Some(token),
            super::pairing::PAIR_CODE_TTL,
            std::time::Duration::from_secs(60),
        ));
        *self
            .pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pairing.clone());
        Some(pairing)
    }

    /// Turn pairing off again.
    pub fn disable_pairing(&self) {
        *self
            .pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// The identity an external token authenticates as.
pub fn external_identity() -> AuthIdentity {
    AuthIdentity::User {
        id: MAIN_PROFILE_ID.to_owned(),
        role: UserRole::User,
    }
}

/// Whether `identity` on a host-managed server is an external client. The
/// host token is the only admin credential there, so everything else is.
pub fn is_external(state: &AppState, identity: Option<&AuthIdentity>) -> bool {
    state.host_managed.is_some() && !matches!(identity, Some(AuthIdentity::Admin))
}

/// A host-owned app-peer session (`peer-<slug>` or `peerctx-<slug>.<ctx>`),
/// whose approvals and questions only the host (the person, in the app's own
/// UI) answers. See UPCR-2026-034 "Approvals belong to the person".
pub fn is_peer_session(session_id: &SessionKey) -> bool {
    session_id.topic().is_some_and(|topic| {
        topic.starts_with("peer-")
            || topic.starts_with(crate::peers::app_binding::PEER_CONTEXT_TOPIC_PREFIX)
    })
}

/// The refusal an external answer to a host-owned peer gets.
pub fn peer_answer_denied(what: &str) -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(format!(
        "an external client cannot answer a host-owned app peer's {what}; answer it in the app"
    ))
    .with_data(serde_json::json!({ "kind": HOST_OWNED_PEER_ANSWER_DENIED }))
}

/// Outermost guard: refuse a request whose `Host` does not name the loopback
/// listener (DNS rebinding). A no-op unless host-managed.
pub(crate) async fn host_header_guard(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    if let Some(host_managed) = &state.host_managed {
        let authority = req.uri().authority().map(|a| a.as_str().to_owned());
        if !host_managed.host_allowed(req.headers(), authority.as_deref()) {
            tracing::warn!(
                target: "octos::api::host_managed",
                "rejected request with a Host header that does not name the loopback listener"
            );
            return (StatusCode::MISDIRECTED_REQUEST, "unexpected Host").into_response();
        }
    }
    next.run(req).await
}

/// `POST /api/admin/host/pairing` (host token only): mint a one-time code a
/// loopback web client exchanges for the EXTERNAL token at `/pair/claim`.
/// The host calls this only while its pairing UI is open.
pub(crate) async fn enable_pairing(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<AuthIdentity>>,
) -> Response {
    let Some(host_managed) = &state.host_managed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    enable_pairing_audited(host_managed, |_| {
        super::admin_audit::record_admin_action(
            &state,
            identity.as_ref().map(|identity| &identity.0),
            "host.pairing.enable",
            "external",
            None,
            Some(serde_json::json!({ "expires_in_secs": super::pairing::PAIR_CODE_TTL.as_secs() })),
        )
    })
}

/// Mint a code, audit it, and answer; an unauditable code is turned off
/// again before anyone can see it (fail closed).
fn enable_pairing_audited(
    host_managed: &HostManaged,
    audit: impl FnOnce(&PairingState) -> eyre::Result<()>,
) -> Response {
    let Some(pairing) = host_managed.enable_pairing() else {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "error": { "kind": "external_access_disabled" } })),
        )
            .into_response();
    };
    // Audited without the code: who enabled pairing, and for how long.
    if let Err(error) = audit(&pairing) {
        host_managed.disable_pairing();
        tracing::error!(%error, "could not audit the pairing; pairing stays off");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    axum::Json(serde_json::json!({
        "code": pairing.printed_code(),
        "server_origin": pairing.server_origin(),
        "expires_in_secs": super::pairing::PAIR_CODE_TTL.as_secs(),
    }))
    .into_response()
}

/// `DELETE /api/admin/host/pairing` (host token only): pairing off.
pub(crate) async fn disable_pairing(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<AuthIdentity>>,
) -> Response {
    let Some(host_managed) = &state.host_managed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    host_managed.disable_pairing();
    // Off regardless; a failed audit write is logged, not undone.
    if let Err(error) = super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "host.pairing.disable",
        "external",
        None,
        None,
    ) {
        tracing::error!(%error, "could not audit turning pairing off");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Read the two token lines the host writes first on stdin: the host token,
/// then the external token (an empty line: no external clients). The rest of
/// stdin is the lifeline ([`spawn_stdin_eof_watcher`]). Fails after
/// `timeout` or at EOF before the first line.
pub(crate) fn read_tokens_from_stdin(
    stdin: std::io::Stdin,
    timeout: std::time::Duration,
) -> eyre::Result<(Option<String>, Option<String>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("octos-host-tokens".into())
        .spawn(move || {
            let _ = tx.send(read_token_lines(&mut stdin.lock()));
        })?;
    rx.recv_timeout(timeout)
        .map_err(|_| eyre::eyre!("--host-managed: the host sent no tokens on stdin"))?
}

fn read_token_lines(
    input: &mut impl std::io::BufRead,
) -> eyre::Result<(Option<String>, Option<String>)> {
    let mut line = || -> eyre::Result<Option<String>> {
        let mut text = String::new();
        let read = input.read_line(&mut text)?;
        let token = text.trim_end_matches(['\r', '\n']).to_owned();
        Ok((read > 0 && !token.is_empty()).then_some(token))
    };
    let host = line()?;
    eyre::ensure!(
        host.is_some(),
        "--host-managed: the host token line is missing"
    );
    Ok((host, line()?))
}

/// Stop the server when stdin reaches EOF: the host exited or closed its end.
/// Bytes read are ignored. Runs on a plain thread (stdin reads block).
pub(crate) fn spawn_stdin_eof_watcher(stop: Arc<tokio::sync::watch::Sender<bool>>) {
    let spawned = std::thread::Builder::new()
        .name("octos-host-stdin".into())
        .spawn(move || {
            wait_for_eof(std::io::stdin().lock());
            tracing::info!(
                target: "octos::api::host_managed",
                "host closed stdin; stopping"
            );
            stop.send_replace(true);
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "could not watch stdin; stopping instead of running unowned");
        // Fail closed: an unwatched host-managed server could outlive its host.
        std::process::exit(1);
    }
}

fn wait_for_eof(mut input: impl std::io::Read) {
    let mut buf = [0u8; 256];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Linux/Android: ask the kernel for SIGTERM when the parent dies (SIGTERM
/// takes the same graceful path as `stop`).
///
/// Orphan posture on every platform: stdin EOF is the lifeline. When the
/// host process ends for any reason (exit, crash, SIGKILL, Windows
/// TerminateProcess), the OS closes its end of the pipe and the server
/// stops; `tests/serve_host_managed.rs` SIGKILLs the pipe's holder to prove
/// it. What EOF cannot see is a host whose pipe end outlives it (inherited by
/// another process it spawned). Linux/Android add this parent-death signal
/// for that case; elsewhere the host must not leak the write end (spawn it
/// close-on-exec, as Rust's `std::process` does). The signal follows the
/// parent THREAD that spawned us, so hosts spawn from a long-lived thread.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn bind_to_parent() -> eyre::Result<()> {
    use eyre::WrapErr;
    let parent = rustix::process::getppid();
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
        .wrap_err("could not set the parent-death signal")?;
    // The parent may have died before the signal was armed.
    eyre::ensure!(
        rustix::process::getppid() == parent,
        "the host exited during startup"
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn bind_to_parent() -> eyre::Result<()> {
    Ok(())
}

/// Adopt the listening socket the host passed as descriptor `fd`, so the host
/// keeps the port across server restarts (nobody else can take it while no
/// server runs; connections queue in the backlog). The socket must be TCP and
/// bound to 127.0.0.1; it is made close-on-exec and non-blocking.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn adopt_listener_fd(fd: i32) -> eyre::Result<std::net::TcpListener> {
    use eyre::WrapErr;
    use std::os::fd::{FromRawFd, OwnedFd};
    eyre::ensure!(
        fd > 2,
        "--listen-fd must name an inherited descriptor other than stdin, stdout or stderr"
    );
    // Validate the number names an open descriptor before taking ownership.
    rustix::io::fcntl_getfd(
        // SAFETY: the borrow ends with this call; fcntl(F_GETFD) on a closed
        // number fails with EBADF instead of touching memory.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
    )
    .wrap_err("--listen-fd does not name an open descriptor")?;
    // SAFETY: the descriptor is open (checked above) and was handed to this
    // process by its parent for this exact purpose; nothing else in the
    // process refers to it, so taking ownership cannot double-close.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    eyre::ensure!(
        rustix::net::sockopt::socket_type(&owned).wrap_err("--listen-fd is not a socket")?
            == rustix::net::SocketType::STREAM,
        "--listen-fd is not a stream socket"
    );
    rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC)
        .wrap_err("could not make the inherited listener close-on-exec")?;
    rustix::net::listen(&owned, 1024).wrap_err("the inherited socket cannot listen")?;
    let listener = std::net::TcpListener::from(owned);
    let addr = listener
        .local_addr()
        .wrap_err("the inherited socket has no local address")?;
    eyre::ensure!(
        addr.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) && addr.port() != 0,
        "--listen-fd must be bound to 127.0.0.1 (got {addr})"
    );
    listener
        .set_nonblocking(true)
        .wrap_err("could not make the inherited listener non-blocking")?;
    Ok(listener)
}

#[cfg(not(unix))]
pub(crate) fn adopt_listener_fd(_fd: i32) -> eyre::Result<std::net::TcpListener> {
    eyre::bail!("--listen-fd is supported on Unix only")
}

#[cfg(test)]
#[path = "host_managed_tests.rs"]
mod host_managed_tests;
