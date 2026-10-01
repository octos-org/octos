//! Host-registered tools of a host-owned app peer (UPCR-2026-035).
//!
//! The host declares a peer's app tools and the generic kernel tools the app
//! may use with `peer/tools/register` (host-token authorized). The set is
//! durable (`peers/<slug>/host_tools.json`), versioned, and replaced whole.
//! Once a peer has a set, a host-driven turn of the peer's session or of one
//! of its request contexts keeps the peer's usual kernel tools (exactly the
//! set's `generic_tools` of them when the host sets that list) and ADDS the
//! registered app tools. Any other turn on those sessions gets no tools.
//! The system agent's input to a host-owned peer is delivered to the host
//! as `peer/input` ([`deliver_peer_input`]), never run as a kernel turn.
//!
//! App tool calls go to the host: the kernel sends `peer/tool/call` to the
//! connection that registered the set and waits for `peer/tool/result`
//! (bounded by the set's call timeout; on timeout or turn interrupt it sends
//! `peer/tool/cancel`). Risk gating happens in
//! [`octos_agent::HostRoutedTool`] before the host is asked. Every call is
//! appended to `peers/<slug>/tool_audit.jsonl`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use octos_agent::{
    HostRoutedTool, HostToolAudit, HostToolCall, HostToolCallOutcome, HostToolCaller,
    HostToolConfirm, HostToolDecl, HostToolRisk, HostToolRouter, OccurrenceClaim, ToolRegistry,
};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::app_binding::{PEER_CONTEXT_TOPIC_PREFIX, parse_context_topic, validate_context_id};
use super::{peer_io, peer_slug_is_safe, staged_peer_dir};

/// Peer-dir leaf holding the registered [`PeerHostToolSet`].
pub(crate) const HOST_TOOLS_LEAF: &str = "host_tools.json";
/// Peer-dir leaf of the per-call audit log.
pub(crate) const TOOL_AUDIT_LEAF: &str = "tool_audit.jsonl";

/// Server → host: run an app tool.
pub(crate) const PEER_TOOL_CALL_NOTIFICATION: &str =
    octos_core::ui_protocol::methods::PEER_TOOL_CALL;
/// Server → host: stop a call the kernel no longer waits for.
pub(crate) const PEER_TOOL_CANCEL_NOTIFICATION: &str =
    octos_core::ui_protocol::methods::PEER_TOOL_CANCEL;
/// Server → host: the system agent's input for a host-owned peer.
pub(crate) const PEER_INPUT_NOTIFICATION: &str = octos_core::ui_protocol::methods::PEER_INPUT;

pub(crate) const MAX_APP_TOOLS: usize = 64;
pub(crate) const MAX_GENERIC_TOOLS: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_MODEL_NAME_BYTES: usize = 64;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;
const MAX_CALL_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_APPROVAL_TTL_SECS: u64 = 3_600;
const MAX_APPROVAL_TTL_SECS: u64 = 7 * 24 * 3_600;
const DEFAULT_MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES_CEILING: usize = 1024 * 1024;
/// In-flight host calls per peer.
pub(crate) const MAX_PENDING_CALLS_PER_PEER: usize = 16;
/// How long a claimed approval occurrence is remembered.
const OCCURRENCE_RETENTION: Duration = Duration::from_secs(24 * 3_600);

/// A peer's registered tool set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerHostToolSet {
    /// Starts at 1; every registration increments it.
    pub(crate) version: u64,
    pub(crate) tools: Vec<HostToolDecl>,
    /// The peer's kernel tools, EXACTLY (e.g. `["read_file",
    /// "deep_search"]`), as the host allows from the app's declared and
    /// granted tools. `None`: the peer's usual kernel tools. Only tools the
    /// peer's session has can be offered; the list never adds others.
    #[serde(default)]
    pub(crate) generic_tools: Option<Vec<String>>,
    pub(crate) call_timeout_ms: u64,
    pub(crate) approval_ttl_secs: u64,
    pub(crate) max_result_bytes: usize,
}

// ---------------------------------------------------------------------------
// Registration input
// ---------------------------------------------------------------------------

/// One tool as the host declares it: an entry of the app bundle's
/// `tools.json`, the one declaration source for native modules and script
/// apps alike.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolInput {
    pub(crate) name: String,
    /// The app that owns the tool; defaults to the name's first segment. A
    /// cross-app tool (another app's tool registered on this peer, as the
    /// host granted it) names its owner here.
    #[serde(default)]
    pub(crate) app: Option<String>,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    #[serde(default)]
    pub(crate) output_schema: Option<Value>,
    pub(crate) risk: HostToolRisk,
    #[serde(default)]
    pub(crate) background: bool,
    #[serde(default)]
    pub(crate) outward: bool,
    #[serde(default)]
    pub(crate) confirm: HostToolConfirm,
    /// App Hub metadata the kernel does not act on (callers other than the
    /// app's own agent are a follow-up).
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) shareable: Option<bool>,
}

/// Registration options.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolSetOptions {
    #[serde(default)]
    pub(crate) call_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) approval_ttl_secs: Option<u64>,
    #[serde(default)]
    pub(crate) max_result_bytes: Option<usize>,
}

fn segment_is_valid(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// An owning app id: `[a-z][a-z0-9_.-]{0,63}`.
fn validate_app_id(app: &str) -> Result<(), String> {
    let bytes = app.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(format!("app '{app}' must match [a-z][a-z0-9_.-]{{0,63}}"))
    }
}

/// `<app>.<tool>[.<more>]`: 2–4 segments of `[a-z][a-z0-9_]{0,31}`.
pub(crate) fn validate_app_tool_name(name: &str) -> Result<(), String> {
    let segments: Vec<&str> = name.split('.').collect();
    if !(2..=4).contains(&segments.len()) || !segments.iter().all(|s| segment_is_valid(s)) {
        return Err(format!(
            "tool name '{name}' must be '<app>.<tool>': 2-4 '.'-separated segments of [a-z][a-z0-9_]{{0,31}}"
        ));
    }
    Ok(())
}

/// A kernel tool name: `[A-Za-z0-9_-]`, no `.` (app tools are dotted).
fn validate_generic_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_MODEL_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "generic tool name '{name}' is not a kernel tool name"
        ))
    }
}

fn parse_schema(tool: &str, field: &str, schema: Value) -> Result<Value, String> {
    let size = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
    if size > MAX_SCHEMA_BYTES {
        return Err(format!(
            "{tool}: {field} is {size} bytes (max {MAX_SCHEMA_BYTES})"
        ));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "{tool}: {field} must be a JSON Schema object (\"type\": \"object\")"
        ));
    }
    check_schema_shape(&schema, 0).map_err(|err| format!("{tool}: {field}: {err}"))?;
    Ok(schema)
}

const MAX_SCHEMA_DEPTH: usize = 10;
const SCHEMA_TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Structural check against the JSON Schema meta-schema's shapes for the
/// keywords that matter here: `type`, `properties`, `required`, `items`,
/// `enum`, the combinators, and a nesting depth of 10.
fn check_schema_shape(schema: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("nested deeper than {MAX_SCHEMA_DEPTH}"));
    }
    if depth > 0 && schema.is_boolean() {
        return Ok(());
    }
    let Some(object) = schema.as_object() else {
        return Err("a schema must be a JSON object".into());
    };
    if let Some(additional) = object.get("additionalProperties") {
        if !additional.is_boolean() {
            check_schema_shape(additional, depth + 1)
                .map_err(|err| format!("additionalProperties: {err}"))?;
        }
    }
    let valid_type = |t: &Value| t.as_str().is_some_and(|t| SCHEMA_TYPES.contains(&t));
    match object.get("type") {
        None => {}
        Some(Value::Array(types)) if !types.is_empty() && types.iter().all(valid_type) => {}
        Some(t) if valid_type(t) => {}
        Some(other) => return Err(format!("\"type\" {other} is not a JSON Schema type")),
    }
    if let Some(properties) = object.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err("\"properties\" must be an object".into());
        };
        for (name, sub) in properties {
            check_schema_shape(sub, depth + 1).map_err(|err| format!("{name}: {err}"))?;
        }
    }
    if let Some(required) = object.get("required") {
        if !required
            .as_array()
            .is_some_and(|r| r.iter().all(Value::is_string))
        {
            return Err("\"required\" must be an array of strings".into());
        }
    }
    if let Some(items) = object.get("items") {
        match items {
            Value::Array(list) => {
                for sub in list {
                    check_schema_shape(sub, depth + 1)?;
                }
            }
            Value::Bool(_) => {}
            sub => check_schema_shape(sub, depth + 1)?,
        }
    }
    if let Some(values) = object.get("enum") {
        if !values.is_array() {
            return Err("\"enum\" must be an array".into());
        }
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(list) = object.get(combinator) {
            let Some(list) = list.as_array() else {
                return Err(format!("\"{combinator}\" must be an array"));
            };
            for sub in list {
                check_schema_shape(sub, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Validate a registration and build the next set (`version` is filled by
/// the caller).
pub(crate) fn build_tool_set(
    flat: Vec<ToolInput>,
    generic_tools: Option<Vec<String>>,
    options: ToolSetOptions,
) -> Result<PeerHostToolSet, String> {
    if flat.len() > MAX_APP_TOOLS {
        return Err(format!("{} app tools (max {MAX_APP_TOOLS})", flat.len()));
    }
    let generic = match generic_tools {
        None => None,
        Some(names) => {
            if names.len() > MAX_GENERIC_TOOLS {
                return Err(format!(
                    "{} generic tools (max {MAX_GENERIC_TOOLS})",
                    names.len()
                ));
            }
            let mut generic: Vec<String> = Vec::new();
            for name in names {
                validate_generic_name(&name)?;
                if !generic.contains(&name) {
                    generic.push(name);
                }
            }
            Some(generic)
        }
    };
    let mut seen_model_names: Vec<String> = generic.clone().unwrap_or_default();
    let mut decls = Vec::with_capacity(flat.len());
    for tool in flat {
        validate_app_tool_name(&tool.name)?;
        let model_name = tool.name.replace('.', "_");
        if model_name.len() > MAX_MODEL_NAME_BYTES {
            return Err(format!("tool name '{}' is too long", tool.name));
        }
        // A host tool never takes a kernel tool's name: the model, the audit
        // and every name-based filter must be able to tell them apart.
        if octos_agent::tools::RESERVED_BUILTIN_TOOL_NAMES.contains(&model_name.as_str()) {
            return Err(format!(
                "tool '{}' would be seen by the model as the kernel tool '{model_name}'",
                tool.name
            ));
        }
        if seen_model_names.contains(&model_name) {
            return Err(format!(
                "tool '{}' collides with another tool the model would see as '{model_name}'",
                tool.name
            ));
        }
        seen_model_names.push(model_name.clone());
        let description = tool.description.trim().to_owned();
        if description.is_empty() || description.len() > MAX_DESCRIPTION_BYTES {
            return Err(format!(
                "{}: description must be 1..={MAX_DESCRIPTION_BYTES} bytes",
                tool.name
            ));
        }
        let input_schema = parse_schema(&tool.name, "input_schema", tool.input_schema)?;
        let output_schema = match tool.output_schema {
            Some(Value::Null) | None => None,
            Some(raw) => Some(parse_schema(&tool.name, "output_schema", raw)?),
        };
        let app = match tool.app {
            Some(app) => {
                validate_app_id(&app)?;
                app
            }
            None => tool.name.split('.').next().unwrap_or_default().to_owned(),
        };
        decls.push(HostToolDecl {
            name: tool.name,
            app,
            model_name,
            description,
            input_schema,
            output_schema,
            risk: tool.risk,
            background: tool.background,
            outward: tool.outward,
            confirm: tool.confirm,
        });
    }
    let call_timeout_ms = options
        .call_timeout_ms
        .unwrap_or(DEFAULT_CALL_TIMEOUT_MS)
        .clamp(1, MAX_CALL_TIMEOUT_MS);
    let approval_ttl_secs = options
        .approval_ttl_secs
        .unwrap_or(DEFAULT_APPROVAL_TTL_SECS)
        .clamp(1, MAX_APPROVAL_TTL_SECS);
    let max_result_bytes = options
        .max_result_bytes
        .unwrap_or(DEFAULT_MAX_RESULT_BYTES)
        .clamp(1, MAX_RESULT_BYTES_CEILING);
    Ok(PeerHostToolSet {
        version: 0,
        tools: decls,
        generic_tools: generic,
        call_timeout_ms,
        approval_ttl_secs,
        max_result_bytes,
    })
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// What the kernel knows about a peer's registered set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StoredToolSet {
    /// Never registered: the peer keeps the ordinary tool roster.
    None,
    Registered(PeerHostToolSet),
    /// The leaf exists but cannot be read: fail closed (no tools at all).
    Unreadable,
}

pub(crate) fn read_tool_set(peers_root: &Path, slug: &str) -> StoredToolSet {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return StoredToolSet::None;
    };
    if dir.join(HOST_TOOLS_LEAF).symlink_metadata().is_err() {
        return StoredToolSet::None;
    }
    match peer_io::read_peer_file(&dir, HOST_TOOLS_LEAF, peer_io::PEER_FILE_READ_CAP_LARGE)
        .and_then(|body| serde_json::from_str::<PeerHostToolSet>(&body).ok())
    {
        Some(set) => StoredToolSet::Registered(set),
        None => StoredToolSet::Unreadable,
    }
}

/// Durably replace the set. The caller serializes registrations per peer.
pub(crate) fn write_tool_set(
    peers_root: &Path,
    slug: &str,
    set: &PeerHostToolSet,
) -> Result<(), String> {
    let dir = staged_peer_dir(peers_root, slug)
        .ok_or_else(|| format!("peer '{slug}' is not a staged peer"))?;
    let body = serde_json::to_string(set).map_err(|err| err.to_string())?;
    if body.len() > peer_io::PEER_FILE_READ_CAP_LARGE {
        return Err(format!(
            "the tool set is {} bytes (max {})",
            body.len(),
            peer_io::PEER_FILE_READ_CAP_LARGE
        ));
    }
    peer_io::write_peer_file_durable(&dir, HOST_TOOLS_LEAF, &body)
        .map_err(|err| format!("failed to record the tool set: {err}"))
}

/// Serializes read-modify-write of ONE peer's set (version bump); other
/// peers' registrations never wait on it.
pub(crate) fn registration_lock(peers_root: &Path, slug: &str) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(route_key(peers_root, slug))
        .or_default()
        .clone()
}

// ---------------------------------------------------------------------------
// Per-session enforcement
// ---------------------------------------------------------------------------

/// The tool roster a session's turns must use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SessionHostTools {
    /// Not a host peer with a registered set: unchanged roster.
    Unrestricted,
    Enforced {
        slug: String,
        context_id: Option<String>,
        set: PeerHostToolSet,
    },
    /// A registered set that cannot be read: offer no tools.
    FailClosed { slug: String },
}

/// Resolve the roster for `session` (a `peer-<slug>` of a host-bound peer or
/// a `peerctx-<slug>.<context>` request context).
pub(crate) fn resolve_session_host_tools(
    peers_root: &Path,
    session: &SessionKey,
) -> SessionHostTools {
    let Some(topic) = session.topic() else {
        return SessionHostTools::Unrestricted;
    };
    let (slug, context_id) = if topic.starts_with(PEER_CONTEXT_TOPIC_PREFIX) {
        match parse_context_topic(topic) {
            Some((slug, context))
                if validate_context_id(context).is_ok() && peer_slug_is_safe(slug) =>
            {
                (slug, Some(context.to_owned()))
            }
            // Never an ordinary roster for a malformed context topic.
            _ => {
                return SessionHostTools::FailClosed {
                    slug: String::new(),
                };
            }
        }
    } else if let Some(slug) = topic.strip_prefix("peer-") {
        (slug, None)
    } else {
        return SessionHostTools::Unrestricted;
    };
    if !peer_slug_is_safe(slug) || !super::app_binding::peer_is_host_owned(peers_root, slug) {
        return SessionHostTools::Unrestricted;
    }
    // The topic alone does not identify the caller: any client of the profile
    // can name `<its own base>#peerctx-<slug>.<id>`. Only sessions on the
    // peer originator's base key (the host's) are the app's sessions; every
    // other one gets no tools at all.
    if !session_is_on_originator_base(peers_root, slug, session) {
        return SessionHostTools::FailClosed {
            slug: slug.to_owned(),
        };
    }
    match read_tool_set(peers_root, slug) {
        StoredToolSet::None => SessionHostTools::Unrestricted,
        StoredToolSet::Registered(set) => SessionHostTools::Enforced {
            slug: slug.to_owned(),
            context_id,
            set,
        },
        StoredToolSet::Unreadable => SessionHostTools::FailClosed {
            slug: slug.to_owned(),
        },
    }
}

/// Whether `session` shares the base key of peer `slug`'s recorded
/// originator. A missing or unreadable originator is `false`.
pub(crate) fn session_is_on_originator_base(
    peers_root: &Path,
    slug: &str,
    session: &SessionKey,
) -> bool {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return false;
    };
    match peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL) {
        Some(recorded) => {
            let originator = SessionKey(recorded.trim().to_owned());
            !originator.0.is_empty() && originator.base_key() == session.base_key()
        }
        None => false,
    }
}

/// Make `registry` offer exactly the resolved roster for one turn of
/// `session_id`. App tools route through a [`TurnHostToolRouter`].
pub(crate) fn apply_session_host_tools(
    registry: &mut ToolRegistry,
    resolved: &SessionHostTools,
    peers_root: &Path,
    session_id: &SessionKey,
    turn_id: &str,
    turn_connection: Option<u64>,
) {
    match resolved {
        SessionHostTools::Unrestricted => {}
        SessionHostTools::FailClosed { .. } => registry.retain(|_| false),
        SessionHostTools::Enforced {
            slug,
            context_id,
            set,
        } => {
            // The base key names the host but is not a secret. Only a turn
            // driven by the connection that registered the set (and holds
            // the host token) is the host's; any other connection's turn on
            // the same topic gets no tools, so it can neither call an app
            // tool nor receive one of its approvals.
            if turn_connection.is_none()
                || host_route_connection(peers_root, slug) != turn_connection
            {
                registry.retain(|_| false);
                return;
            }
            // Registration ADDS the app's tools: the host's own turns keep
            // the peer's usual kernel tools, or EXACTLY the host's
            // `generic_tools` of them when it sets that list.
            if let Some(allowed) = &set.generic_tools {
                registry.retain(|name| allowed.iter().any(|a| a == name));
            }
            if set.tools.is_empty() {
                return;
            }
            let router: Arc<dyn HostToolRouter> = Arc::new(TurnHostToolRouter {
                peers_root: peers_root.to_path_buf(),
                host: ToolHost::Peer(slug.clone()),
                context_id: context_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.to_owned(),
                version: set.version,
                call_timeout: Duration::from_millis(set.call_timeout_ms),
                approval_ttl: Duration::from_secs(set.approval_ttl_secs),
                max_result_bytes: set.max_result_bytes,
            });
            let ttl = Duration::from_secs(set.approval_ttl_secs);
            // An open request context is one of the app's interactive
            // clients; the peer's own session is not, except for a turn the
            // host started from the kernel's `peer/input` (the system
            // agent's request, made on the person's behalf) and the person's
            // own turn in the shared conversation (`origin: person`).
            let interactive = context_id.is_some()
                || is_peer_input_turn(&route_key(peers_root, slug), turn_id)
                || super::turn_origin::is_person_turn_id(session_id, turn_id);
            for decl in &set.tools {
                // A kernel tool of the same name wins: an app tool never
                // shadows one (the model, the audit and every filter must be
                // able to tell them apart).
                if registry.get(&decl.model_name).is_some() {
                    tracing::warn!(
                        peer = %slug,
                        tool = %decl.name,
                        model_name = %decl.model_name,
                        "not offering an app tool whose name a kernel tool already has"
                    );
                    continue;
                }
                registry.register(
                    HostRoutedTool::new(decl.clone(), router.clone(), ttl, interactive)
                        .with_caller(HostToolCaller {
                            kind: "app_peer".into(),
                            peer: Some(slug.clone()),
                            session_id: session_id.0.clone(),
                            context_id: context_id.clone(),
                        }),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host SESSION tool sets (the system agent calling granted app tools)
// ---------------------------------------------------------------------------

/// A tool set registered on a host session that is not an app peer. It lives
/// as long as the registering connection: the host registers again after
/// every reconnect, as for a peer.
#[derive(Clone)]
struct SessionToolSet {
    connection: u64,
    set: PeerHostToolSet,
}

/// Host session tool sets: session route key → set.
static SESSION_SETS: LazyLock<Mutex<HashMap<String, SessionToolSet>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Why a session registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionRegisterError {
    /// `if_version` did not match; the current version.
    VersionConflict(u64),
}

/// Register (replace) the tool set of the host session `session`, routed to
/// `connection`. Returns `(previous_version, version)`.
pub(crate) fn register_session_tool_set(
    peers_root: &Path,
    session: &SessionKey,
    connection: u64,
    send: HostSend,
    mut set: PeerHostToolSet,
    if_version: Option<u64>,
) -> Result<(u64, u64), SessionRegisterError> {
    let key = session_route_key(peers_root, session);
    let mut sets = SESSION_SETS.lock().unwrap_or_else(|p| p.into_inner());
    let current = sets
        .get(&key)
        .map_or(0, |registered| registered.set.version);
    if let Some(expected) = if_version {
        if expected != current {
            return Err(SessionRegisterError::VersionConflict(current));
        }
    }
    set.version = current + 1;
    let version = set.version;
    sets.insert(key.clone(), SessionToolSet { connection, set });
    drop(sets);
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, HostRoute { connection, send });
    Ok((current, version))
}

/// The route key of the host tool set a call on `session` belongs to: the
/// app peer's for a `peer-`/`peerctx-` session, the session's own when a
/// host session set is registered on it.
pub(crate) fn host_route_for_session(peers_root: &Path, session: &SessionKey) -> Option<String> {
    if let Some(slug) = host_peer_slug_of(session) {
        return Some(route_key(peers_root, slug));
    }
    let key = session_route_key(peers_root, session);
    SESSION_SETS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&key)
        .then_some(key)
}

/// Add the host SESSION tool set of `session_id` (if any) to one turn.
///
/// Only a turn driven by the registering connection gets its app tools: the
/// host's own turns on its session, e.g. the system agent's conversation.
/// Every other turn on the session (an external web client, a kernel
/// continuation) gets no app tool. The set's `generic_tools`, when given,
/// narrows every turn on the session (a host-only tool list for it). `peer-` and
/// `peerctx-` sessions are the peer path's ([`apply_session_host_tools`]).
pub(crate) fn apply_session_owned_host_tools(
    registry: &mut ToolRegistry,
    peers_root: &Path,
    session_id: &SessionKey,
    turn_id: &str,
    turn_connection: Option<u64>,
) {
    if session_id.topic().is_some_and(|topic| {
        topic.starts_with("peer-") || topic.starts_with(PEER_CONTEXT_TOPIC_PREFIX)
    }) {
        return;
    }
    let key = session_route_key(peers_root, session_id);
    let Some(registered) = SESSION_SETS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .cloned()
    else {
        return;
    };
    let set = registered.set;
    // The host's kernel tool list for the session narrows EVERY turn on it
    // while the set is registered (the host's, other clients', kernel
    // wake-ups); it never widens the profile policy.
    if let Some(allowed) = &set.generic_tools {
        registry.retain(|name| allowed.iter().any(|a| a == name));
    }
    if turn_connection.is_none()
        || turn_connection != Some(registered.connection)
        || route_connection_by_key(&key) != turn_connection
    {
        return;
    }
    let router: Arc<dyn HostToolRouter> = Arc::new(TurnHostToolRouter {
        peers_root: peers_root.to_path_buf(),
        host: ToolHost::Session(session_id.clone()),
        context_id: None,
        session_id: session_id.clone(),
        turn_id: turn_id.to_owned(),
        version: set.version,
        call_timeout: Duration::from_millis(set.call_timeout_ms),
        approval_ttl: Duration::from_secs(set.approval_ttl_secs),
        max_result_bytes: set.max_result_bytes,
    });
    let ttl = Duration::from_secs(set.approval_ttl_secs);
    for decl in &set.tools {
        if registry.get(&decl.model_name).is_some() {
            tracing::warn!(
                session = %session_id,
                tool = %decl.name,
                "not offering an app tool whose name a kernel tool already has"
            );
            continue;
        }
        // The host drives this session's turns itself (the person's own
        // conversation with the system agent): attended.
        registry.register(
            HostRoutedTool::new(decl.clone(), router.clone(), ttl, true).with_caller(
                HostToolCaller {
                    kind: "system".into(),
                    peer: None,
                    session_id: session_id.0.clone(),
                    context_id: None,
                },
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// Routing to the host
// ---------------------------------------------------------------------------

/// Sends one notification to the host connection; `false` when it is gone.
pub(crate) type HostSend = Arc<dyn Fn(&'static str, Value) -> bool + Send + Sync>;

/// What the audit needs to know about a call after it left the pending set.
#[derive(Clone)]
struct CallMeta {
    route_key: String,
    host: ToolHost,
    app: String,
    context_id: Option<String>,
    session_id: SessionKey,
    turn_id: String,
    version: u64,
    tool: String,
    tool_call_id: String,
    risk: &'static str,
    args_digest: String,
}

/// Process-wide key of an unknown-outcome marker: the tool set's route (the
/// peer, or the host session), the tool and the argument digest. Neither the
/// turn nor the calling session is part of it (#2572): a later turn, or
/// another request context of the same peer, must not resend it either.
fn unknown_key(route_key: &str, tool: &str, args_digest: &str) -> String {
    format!("{route_key}\u{0}{tool}/{args_digest}")
}

/// How long an unknown outcome blocks the same call.
const UNKNOWN_RETENTION: Duration = Duration::from_secs(24 * 3_600);

struct PendingCall {
    meta: CallMeta,
    max_result_bytes: usize,
    tx: tokio::sync::oneshot::Sender<HostToolCallOutcome>,
    /// Woken by an "awaiting confirmation" acknowledgement.
    ack: Arc<tokio::sync::Notify>,
    /// Destructive or outward: only such a call may be acknowledged.
    gated: bool,
    /// The host connection the call was sent to: only it may answer.
    connection: u64,
    /// Fired when the kernel stops waiting early: the turn was interrupted
    /// or the host connection closed.
    cancel: Arc<CallCancel>,
}

/// Early end of a pending call's wait.
#[derive(Default)]
struct CallCancel {
    notify: tokio::sync::Notify,
    reason: Mutex<Option<&'static str>>,
}

impl CallCancel {
    fn fire(&self, reason: &'static str) {
        let mut slot = self.reason.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.notify.notify_one();
    }

    fn reason(&self) -> &'static str {
        self.reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unwrap_or("cancelled")
    }
}

/// Stop waiting for every host call of turn `turn_id` on `session` (the turn
/// was interrupted). Each ends as `cancelled` for the host (`peer/tool/cancel`)
/// and, unless it only read, as an unknown outcome that is not resent.
/// Returns how many calls were cancelled.
///
/// The turn is also remembered as interrupted, under the same lock a call
/// takes to enter the pending set: a call of that turn that had not reached
/// the host yet (its tool task still in a before-tool hook, or its approval
/// answered just before the interrupt) is refused and never sent.
pub(crate) fn cancel_host_calls_for_turn(session: &SessionKey, turn_id: &str) -> usize {
    let pending = HUB.pending.lock().unwrap_or_else(|p| p.into_inner());
    // This turn is always recorded; a full set forgets the OLDEST interrupt
    // instead (thousands of interrupts back, whose tool tasks are long gone).
    let evicted = HUB
        .interrupted_turns
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mark(
            interrupted_turn_key(session, turn_id),
            INTERRUPTED_RETENTION,
        );
    if evicted {
        tracing::warn!(
            session = %session.0,
            "interrupted-turn set full: forgot the oldest interrupt to record this one"
        );
    }
    let mut cancelled = 0;
    for call in pending.values() {
        if call.meta.session_id == *session && call.meta.turn_id == turn_id {
            call.cancel.fire("cancelled");
            cancelled += 1;
        }
    }
    cancelled
}

/// How long an interrupted turn is remembered (its tool tasks are long gone
/// by then).
const INTERRUPTED_RETENTION: Duration = Duration::from_secs(24 * 3_600);

fn interrupted_turn_key(session: &SessionKey, turn_id: &str) -> String {
    format!("{}\u{0}{turn_id}", session.0)
}

/// A peer's tool host: the connection that registered its set.
#[derive(Clone)]
struct HostRoute {
    connection: u64,
    send: HostSend,
}

/// Time-bounded, size-bounded set of claimed keys (oldest evicted first,
/// no full scans).
#[derive(Default)]
struct BoundedClaims {
    order: std::collections::VecDeque<(String, Instant)>,
    keys: std::collections::HashSet<String>,
}

/// Result of claiming a key in a [`BoundedClaims`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Claimed,
    AlreadyClaimed,
    /// The set is full of unexpired claims.
    Full,
}

impl BoundedClaims {
    const MAX: usize = 4_096;

    /// Drop expired claims only (oldest first; no full scan).
    fn evict_expired(&mut self, now: Instant, retention: Duration) {
        while let Some((key, at)) = self.order.front() {
            if now.duration_since(*at) < retention {
                break;
            }
            self.keys.remove(key);
            self.order.pop_front();
        }
    }

    /// Insert `key`. A full set never drops an unexpired claim.
    fn claim(&mut self, key: String, retention: Duration) -> Claim {
        let now = Instant::now();
        self.evict_expired(now, retention);
        if self.keys.contains(&key) {
            return Claim::AlreadyClaimed;
        }
        if self.order.len() >= Self::MAX {
            return Claim::Full;
        }
        self.keys.insert(key.clone());
        self.order.push_back((key, now));
        Claim::Claimed
    }

    /// Insert `key`, evicting the oldest claim when full (for markers whose
    /// loss is preferable to refusing work). `key` itself is always recorded;
    /// returns whether an unexpired claim had to be evicted for it.
    fn mark(&mut self, key: String, retention: Duration) -> bool {
        let now = Instant::now();
        self.evict_expired(now, retention);
        if self.keys.contains(&key) {
            return false;
        }
        let mut evicted = false;
        if self.order.len() >= Self::MAX {
            if let Some((oldest, _)) = self.order.pop_front() {
                self.keys.remove(&oldest);
                evicted = true;
            }
        }
        self.keys.insert(key.clone());
        self.order.push_back((key, now));
        evicted
    }

    fn contains(&mut self, key: &str, retention: Duration) -> bool {
        self.evict_expired(Instant::now(), retention);
        self.keys.contains(key)
    }

    fn remove(&mut self, key: &str) {
        if self.keys.remove(key) {
            self.order.retain(|(k, _)| k != key);
        }
    }
}

/// Calls the kernel stopped waiting for, kept so a late result is audited.
const FINISHED_RETENTION: Duration = Duration::from_secs(3_600);
const FINISHED_MAX: usize = 1_024;

#[derive(Default)]
struct HostToolHub {
    routes: Mutex<HashMap<String, HostRoute>>,
    /// `peer/input` deliveries already sent: `(route, input id)`.
    inputs: Mutex<BoundedClaims>,
    /// Turn ids the kernel handed out in `peer/input`: `(route, turn id)`.
    /// The host starting such a turn runs the system agent's request, on
    /// the person's behalf: it counts as attended.
    input_turns: Mutex<BoundedClaims>,
    /// `(session, turn)` pairs interrupted by the person: no call of theirs is
    /// sent to the host any more. Written and read under the `pending` lock.
    interrupted_turns: Mutex<BoundedClaims>,
    pending: Mutex<HashMap<String, PendingCall>>,
    finished: Mutex<HashMap<String, (CallMeta, Instant)>>,
    occurrences: Mutex<BoundedClaims>,
    /// `(route, tool, args digest)` whose outcome is unknown.
    unknown: Mutex<BoundedClaims>,
}

static HUB: LazyLock<HostToolHub> = LazyLock::new(HostToolHub::default);

/// Process-wide key of one peer's route.
pub(crate) fn route_key(peers_root: &Path, slug: &str) -> String {
    format!("{}\u{0}{slug}", peers_root.display())
}

/// Process-wide key of a host SESSION's route (a tool set registered on a
/// session that is not an app peer, e.g. the system agent's conversation).
/// Never equal to a peer's key: a slug has no NUL.
pub(crate) fn session_route_key(peers_root: &Path, session: &SessionKey) -> String {
    format!("{}\u{0}session\u{0}{}", peers_root.display(), session.0)
}

/// Whose tool set a call belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolHost {
    /// A host-owned app peer (its session and request contexts).
    Peer(String),
    /// A host session that is not a peer (e.g. the system agent's).
    Session(SessionKey),
}

impl ToolHost {
    pub(crate) fn route_key(&self, peers_root: &Path) -> String {
        match self {
            Self::Peer(slug) => route_key(peers_root, slug),
            Self::Session(session) => session_route_key(peers_root, session),
        }
    }

    pub(crate) fn peer(&self) -> Option<&str> {
        match self {
            Self::Peer(slug) => Some(slug),
            Self::Session(_) => None,
        }
    }

    /// `caller.kind` of a call made through this set.
    fn caller_kind(&self) -> &'static str {
        match self {
            Self::Peer(_) => "app_peer",
            Self::Session(_) => "system",
        }
    }

    fn audit(&self, peers_root: &Path, row: &Value) {
        match self {
            Self::Peer(slug) => append_audit(peers_root, slug, row),
            Self::Session(_) => append_session_audit(peers_root, row),
        }
    }
}

/// Audit leaf (in the profile data dir) of calls made through host SESSION
/// tool sets.
pub(crate) const SESSION_TOOL_AUDIT_LEAF: &str = "host_session_tool_audit.jsonl";

fn append_session_audit(peers_root: &Path, row: &Value) {
    let Some(dir) = peers_root.parent() else {
        return;
    };
    let size = std::fs::symlink_metadata(dir.join(SESSION_TOOL_AUDIT_LEAF))
        .map(|m| m.len())
        .unwrap_or(0);
    if size >= AUDIT_MAX_BYTES {
        return;
    }
    if let Err(error) = peer_io::append_peer_line(dir, SESSION_TOOL_AUDIT_LEAF, &format!("{row}\n"))
    {
        tracing::warn!(%error, "failed to append the host session tool audit row");
    }
}

/// Route the peer's app tool calls to `send` on `connection` (the
/// registering connection), replacing any earlier route. Only turns driven by
/// that connection get the peer's tools.
pub(crate) fn set_host_route(peers_root: &Path, slug: &str, connection: u64, send: HostSend) {
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(route_key(peers_root, slug), HostRoute { connection, send });
}

/// How long a delivered `peer/input` is remembered for deduplication.
const INPUT_RETENTION: Duration = Duration::from_secs(24 * 3_600);

/// Whether `turn_id` is a turn the kernel handed out in a `peer/input` of
/// the route `route_key`.
fn is_peer_input_turn(route_key: &str, turn_id: &str) -> bool {
    HUB.input_turns
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&format!("{route_key}\u{0}{turn_id}"), INPUT_RETENTION)
}

/// Whether `turn_id` is the turn the kernel handed out in a `peer/input` of
/// the host-owned peer `slug` (a turn started with it is the system agent's).
pub(crate) fn peer_input_turn(peers_root: &Path, slug: &str, turn_id: &str) -> bool {
    is_peer_input_turn(&route_key(peers_root, slug), turn_id)
}

/// Result of [`deliver_peer_input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerInputDelivery {
    /// Sent to the host connection.
    Sent,
    /// This input (same id) was already sent; nothing was sent again.
    AlreadySent,
}

/// The session a host-owned peer's own turns run on:
/// `<originator base>#peer-<slug>`.
pub(crate) fn host_peer_session(peers_root: &Path, slug: &str) -> Option<SessionKey> {
    let dir = staged_peer_dir(peers_root, slug)?;
    let recorded = peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    let originator = SessionKey(recorded.trim().to_owned());
    (!originator.0.is_empty()).then(|| SessionKey(format!("{}#peer-{slug}", originator.base_key())))
}

/// Deliver the system agent's input for the host-owned peer `slug` to the
/// peer's host connection as `peer/input`. The host starts the turn itself,
/// so it runs host-driven: with the peer's tools and the app's approval
/// routing. A host-owned peer never runs a kernel-internal turn for input.
///
/// Fails, and sends nothing, when no host connection holds the peer's route
/// (it never registered, or it disconnected). `input_id` deduplicates: the
/// same input is sent at most once.
pub(crate) fn deliver_peer_input(
    peers_root: &Path,
    slug: &str,
    input_id: &str,
    text: &str,
) -> Result<PeerInputDelivery, String> {
    let not_connected = || {
        format!(
            "the app that owns peer '{slug}' is not connected, so the input was not \
             delivered; try again once the app is open"
        )
    };
    let session = host_peer_session(peers_root, slug)
        .ok_or_else(|| format!("peer '{slug}' has no recorded originator"))?;
    let key = route_key(peers_root, slug);
    let (send, connection) = HUB
        .routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .map(|route| (route.send.clone(), route.connection))
        .ok_or_else(not_connected)?;
    let claim_key = format!("{key}\u{0}{input_id}");
    match HUB
        .inputs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .claim(claim_key.clone(), INPUT_RETENTION)
    {
        Claim::Claimed => {}
        Claim::AlreadyClaimed => return Ok(PeerInputDelivery::AlreadySent),
        Claim::Full => {
            return Err(format!(
                "too many recent inputs for peer '{slug}'; try again later"
            ));
        }
    }
    let turn_id = octos_core::ui_protocol::TurnId::new();
    HUB.input_turns
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mark(format!("{key}\u{0}{}", turn_id.0), INPUT_RETENTION);
    // Recorded before the send: the host may refuse it at once.
    let entry_at = INPUT_LEDGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(input_id, &key, connection, &turn_id.0.to_string());
    let params = json!({
        "peer": slug,
        "session_id": session,
        "input_id": input_id,
        "turn_id": turn_id,
        "text": text,
    });
    if send(PEER_INPUT_NOTIFICATION, params) {
        append_audit(
            peers_root,
            slug,
            &json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "peer": slug,
                "session_id": session,
                "turn_id": turn_id,
                "input_id": input_id,
                "decision": "peer_input_sent",
            }),
        );
        return Ok(PeerInputDelivery::Sent);
    }
    INPUT_LEDGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(input_id, entry_at);
    HUB.inputs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&claim_key);
    drop_route_if(&key, &send);
    Err(not_connected())
}

// ---------------------------------------------------------------------------
// Answers to `peer/input`: the host starts the turn, or refuses the input
// ---------------------------------------------------------------------------

/// Longest `message` a host may give with `peer/input/reject`.
pub(crate) const PEER_INPUT_REJECT_MESSAGE_MAX_BYTES: usize = 256;

/// How long the system agent's `peer_send_input` waits for the host's answer
/// to a `peer/input`: a `turn/start` with its turn id ends the wait at once,
/// `peer/input/reject` makes the call fail with the reason. A host that does
/// neither leaves the call reporting the input as sent, as before.
pub(crate) const PEER_INPUT_ANSWER_WAIT: Duration = Duration::from_secs(5);

/// Why the host refused a `peer/input` (a closed set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerInputRejectReason {
    /// The account is signed out or suspended.
    SignedOut,
    /// The person has not granted the app consent.
    NoConsent,
    /// The peer is busy past the host's queue limit.
    Busy,
    /// Anything else; `message` says what.
    Other,
}

impl PeerInputRejectReason {
    pub(crate) fn parse(reason: &str) -> Option<Self> {
        match reason {
            "signed_out" => Some(Self::SignedOut),
            "no_consent" => Some(Self::NoConsent),
            "busy" => Some(Self::Busy),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SignedOut => "signed_out",
            Self::NoConsent => "no_consent",
            Self::Busy => "busy",
            Self::Other => "other",
        }
    }
}

/// A host's refusal of one `peer/input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerInputRejection {
    pub(crate) reason: PeerInputRejectReason,
    /// Only with [`PeerInputRejectReason::Other`]; one line, bounded.
    pub(crate) message: Option<String>,
}

impl PeerInputRejection {
    /// Validate a host's `reason` and `message`.
    pub(crate) fn parse(reason: &str, message: Option<String>) -> Result<Self, String> {
        let reason = PeerInputRejectReason::parse(reason).ok_or_else(|| {
            format!("unknown reason '{reason}' (signed_out, no_consent, busy or other)")
        })?;
        let message = match (reason, message) {
            (PeerInputRejectReason::Other, Some(message)) => {
                let message = message.trim().to_owned();
                if message.is_empty() || message.len() > PEER_INPUT_REJECT_MESSAGE_MAX_BYTES {
                    return Err(format!(
                        "message must be 1..={PEER_INPUT_REJECT_MESSAGE_MAX_BYTES} bytes"
                    ));
                }
                if message.chars().any(char::is_control) {
                    return Err("message must be one line without control characters".into());
                }
                Some(message)
            }
            (PeerInputRejectReason::Other, None) => {
                return Err("reason \"other\" needs a message".into());
            }
            (_, Some(_)) => {
                return Err(format!(
                    "message is only given with reason \"other\", not \"{}\"",
                    reason.as_str()
                ));
            }
            (_, None) => None,
        };
        Ok(Self { reason, message })
    }

    /// What the system agent reads: `peer_input_rejected: <reason>` and, for
    /// `other`, the host's message.
    pub(crate) fn describe(&self) -> String {
        match &self.message {
            Some(message) => format!("peer_input_rejected: {} ({message})", self.reason.as_str()),
            None => format!("peer_input_rejected: {}", self.reason.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAnswer {
    Unanswered,
    /// A `turn/start` with the input's turn id arrived.
    Started,
    Rejected,
}

/// One `peer/input` the kernel sent, until the host answers it.
struct InputEntry {
    route_key: String,
    /// The host connection it was sent to: only it may refuse it.
    connection: u64,
    turn_id: String,
    at: Instant,
    answer: InputAnswer,
    /// The system agent's call stopped waiting for the answer.
    call_returned: bool,
    /// A refusal handed to the waiting call.
    for_call: Option<PeerInputRejection>,
    wake: Arc<tokio::sync::Notify>,
}

/// Inputs by id (ids carry the calling session, turn and tool call, so they
/// are unique across peers), bounded like the other claim sets.
#[derive(Default)]
struct InputLedger {
    entries: HashMap<String, InputEntry>,
    /// `(route, turn id)` → input id.
    by_turn: HashMap<String, String>,
    order: std::collections::VecDeque<(String, Instant)>,
}

impl InputLedger {
    const MAX: usize = 4_096;

    fn turn_key(route_key: &str, turn_id: &str) -> String {
        format!("{route_key}\u{0}{turn_id}")
    }

    /// Record a new input; returns its insertion time (its identity for
    /// [`Self::remove`]). The oldest entries go first when full or expired.
    fn insert(
        &mut self,
        input_id: &str,
        route_key: &str,
        connection: u64,
        turn_id: &str,
    ) -> Instant {
        let now = Instant::now();
        while let Some((oldest, at)) = self.order.front().cloned() {
            if self.entries.len() < Self::MAX && now.duration_since(at) < INPUT_RETENTION {
                break;
            }
            self.order.pop_front();
            self.remove(&oldest, at);
        }
        self.remove_any(input_id);
        self.by_turn
            .insert(Self::turn_key(route_key, turn_id), input_id.to_owned());
        self.entries.insert(
            input_id.to_owned(),
            InputEntry {
                route_key: route_key.to_owned(),
                connection,
                turn_id: turn_id.to_owned(),
                at: now,
                answer: InputAnswer::Unanswered,
                call_returned: false,
                for_call: None,
                wake: Arc::new(tokio::sync::Notify::new()),
            },
        );
        self.order.push_back((input_id.to_owned(), now));
        now
    }

    /// Forget `input_id` if it is still the entry recorded at `at`.
    fn remove(&mut self, input_id: &str, at: Instant) {
        if self
            .entries
            .get(input_id)
            .is_some_and(|entry| entry.at == at)
        {
            self.remove_any(input_id);
        }
    }

    fn remove_any(&mut self, input_id: &str) {
        if let Some(entry) = self.entries.remove(input_id) {
            self.by_turn
                .remove(&Self::turn_key(&entry.route_key, &entry.turn_id));
        }
    }
}

static INPUT_LEDGER: LazyLock<Mutex<InputLedger>> =
    LazyLock::new(|| Mutex::new(InputLedger::default()));

/// Wait up to `wait` for the host's answer to the input `input_id`, for the
/// system agent's `peer_send_input` call that sent it. Returns the host's
/// refusal, or `None` when the host started the turn, the wait ran out, or
/// the kernel knows no such input. After this returns (or is dropped) a
/// refusal is reported on the system session instead.
pub(crate) async fn await_peer_input_answer(
    input_id: &str,
    wait: Duration,
) -> Option<PeerInputRejection> {
    /// Marks the call as returned however the wait ends (an interrupted
    /// turn drops it).
    struct Returned<'a>(&'a str);
    impl Drop for Returned<'_> {
        fn drop(&mut self) {
            if let Some(entry) = INPUT_LEDGER
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entries
                .get_mut(self.0)
            {
                entry.call_returned = true;
            }
        }
    }
    let wake = {
        let mut ledger = INPUT_LEDGER.lock().unwrap_or_else(|p| p.into_inner());
        let entry = ledger.entries.get_mut(input_id)?;
        if entry.call_returned {
            return None;
        }
        match entry.answer {
            InputAnswer::Unanswered => entry.wake.clone(),
            InputAnswer::Started => {
                entry.call_returned = true;
                return None;
            }
            InputAnswer::Rejected => {
                entry.call_returned = true;
                return entry.for_call.take();
            }
        }
    };
    let returned = Returned(input_id);
    let _ = tokio::time::timeout(wait, wake.notified()).await;
    let rejection = INPUT_LEDGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entries
        .get_mut(input_id)
        .and_then(|entry| {
            entry.call_returned = true;
            entry.for_call.take()
        });
    drop(returned);
    rejection
}

/// Why a `turn/start` or a `peer/input/reject` was refused.
fn input_error(kind: &'static str, message: String) -> CompleteError {
    CompleteError { kind, message }
}

/// Would a `turn/start` with `turn_id` on the host-owned peer `slug`'s own
/// session be refused because the host refused its input? Changes nothing:
/// the input is answered only by [`start_peer_input_turn`], once the turn is
/// actually admitted, so a start the kernel refuses for any other reason
/// (a turn in progress, a reused turn id, a budget, a bad request) leaves the
/// input open for `peer/input/reject`.
pub(crate) fn check_peer_input_turn(
    peers_root: &Path,
    slug: &str,
    turn_id: &str,
) -> Result<(), CompleteError> {
    let ledger = INPUT_LEDGER.lock().unwrap_or_else(|p| p.into_inner());
    let Some(input_id) = ledger.by_turn.get(&InputLedger::turn_key(
        &route_key(peers_root, slug),
        turn_id,
    )) else {
        return Ok(());
    };
    match ledger.entries.get(input_id).map(|entry| entry.answer) {
        Some(InputAnswer::Rejected) => Err(input_error(
            "peer_input_rejected",
            format!(
                "turn '{turn_id}' was handed out for input '{input_id}', which the app refused; \
                 it cannot be started"
            ),
        )),
        _ => Ok(()),
    }
}

/// A `turn/start` on the host-owned peer `slug`'s own session with
/// `turn_id`, at the moment it is admitted. If the kernel handed that turn id out in a `peer/input`, the
/// input counts as answered (it can no longer be refused), unless the host
/// already refused it: then its turn id is released and the start is
/// refused (`peer_input_rejected`).
pub(crate) fn start_peer_input_turn(
    peers_root: &Path,
    slug: &str,
    turn_id: &str,
) -> Result<(), CompleteError> {
    let mut ledger = INPUT_LEDGER.lock().unwrap_or_else(|p| p.into_inner());
    let Some(input_id) = ledger
        .by_turn
        .get(&InputLedger::turn_key(
            &route_key(peers_root, slug),
            turn_id,
        ))
        .cloned()
    else {
        return Ok(());
    };
    let Some(entry) = ledger.entries.get_mut(&input_id) else {
        return Ok(());
    };
    match entry.answer {
        InputAnswer::Rejected => Err(input_error(
            "peer_input_rejected",
            format!(
                "turn '{turn_id}' was handed out for input '{input_id}', which the app refused; \
                 it cannot be started"
            ),
        )),
        InputAnswer::Started => Ok(()),
        InputAnswer::Unanswered => {
            entry.answer = InputAnswer::Started;
            entry.wake.notify_one();
            Ok(())
        }
    }
}

/// Where a refusal reached the system agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectDelivery {
    /// Its `peer_send_input` call was still waiting: the call fails with it.
    Call,
    /// The call had returned: recorded on the peer's blackboard and reported
    /// to the system session at its next turn.
    SystemSession,
}

impl RejectDelivery {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::SystemSession => "system_session",
        }
    }
}

/// `peer/input/reject`: the host connection `connection` refuses the input
/// `input_id` it received for the peer `slug`. Accepted once, only from the
/// connection the input was sent to, and only while no `turn/start` with the
/// input's turn id arrived. The turn id is released: it no longer counts as
/// the system agent's request, and a later `turn/start` with it is refused.
pub(crate) fn reject_peer_input(
    peers_root: &Path,
    slug: &str,
    input_id: &str,
    connection: u64,
    rejection: PeerInputRejection,
) -> Result<RejectDelivery, CompleteError> {
    let key = route_key(peers_root, slug);
    let (delivery, turn_id) = {
        let mut ledger = INPUT_LEDGER.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = ledger
            .entries
            .get_mut(input_id)
            .filter(|entry| entry.route_key == key)
        else {
            return Err(input_error(
                "peer_input_not_found",
                format!("peer '{slug}' has no input '{input_id}' (unknown or expired)"),
            ));
        };
        if entry.connection != connection {
            return Err(input_error(
                "peer_input_wrong_connection",
                format!("input '{input_id}' was sent to another connection; only it may refuse it"),
            ));
        }
        match entry.answer {
            InputAnswer::Unanswered => {}
            InputAnswer::Started => {
                return Err(input_error(
                    "peer_input_already_started",
                    format!("the turn of input '{input_id}' was already started"),
                ));
            }
            InputAnswer::Rejected => {
                return Err(input_error(
                    "peer_input_already_rejected",
                    format!("input '{input_id}' was already refused"),
                ));
            }
        }
        entry.answer = InputAnswer::Rejected;
        let waiting = !entry.call_returned && entry.at.elapsed() < PEER_INPUT_ANSWER_WAIT;
        let delivery = if waiting {
            entry.for_call = Some(rejection.clone());
            entry.wake.notify_one();
            RejectDelivery::Call
        } else {
            RejectDelivery::SystemSession
        };
        (delivery, entry.turn_id.clone())
    };
    // Released: a turn with this id is no longer the system agent's request.
    HUB.input_turns
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&InputLedger::turn_key(&key, &turn_id));
    if delivery == RejectDelivery::SystemSession {
        record_input_rejection(peers_root, slug, input_id, &turn_id, &rejection);
    }
    append_audit(
        peers_root,
        slug,
        &json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "peer": slug,
            "session_id": host_peer_session(peers_root, slug),
            "turn_id": turn_id,
            "input_id": input_id,
            "decision": "peer_input_rejected",
            "outcome": rejection.reason.as_str(),
            "message": rejection.message,
            "reported_to": delivery.as_str(),
        }),
    );
    Ok(delivery)
}

/// Peer-dir leaf of the refusals reported after the call returned (the
/// peer's blackboard): one JSON object per line.
pub(crate) const INPUT_REJECTIONS_LEAF: &str = "input_rejections.jsonl";
/// How many lines of [`INPUT_REJECTIONS_LEAF`] the system session was told.
const INPUT_REJECTIONS_CURSOR_LEAF: &str = ".input_rejections_notified";
const INPUT_REJECTIONS_MAX_BYTES: u64 = 256 * 1024;
/// Refusals named in one turn-start note.
const INPUT_REJECTIONS_NOTE_MAX: usize = 8;

fn record_input_rejection(
    peers_root: &Path,
    slug: &str,
    input_id: &str,
    turn_id: &str,
    rejection: &PeerInputRejection,
) {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return;
    };
    let size = std::fs::symlink_metadata(dir.join(INPUT_REJECTIONS_LEAF))
        .map(|m| m.len())
        .unwrap_or(0);
    if size >= INPUT_REJECTIONS_MAX_BYTES {
        tracing::warn!(slug, "input rejection log full; not recording the refusal");
        return;
    }
    let row = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "input_id": input_id,
        "turn_id": turn_id,
        "reason": rejection.reason.as_str(),
        "message": rejection.message,
    });
    if let Err(error) = peer_io::append_peer_line(&dir, INPUT_REJECTIONS_LEAF, &format!("{row}\n"))
    {
        tracing::warn!(slug, %error, "failed to record the peer input refusal");
    }
}

/// Turn-start note for `session`: the refusals of its peers' inputs that
/// arrived after its `peer_send_input` calls had returned, each reported
/// once. `None` for peer sessions and when there is nothing new.
pub(crate) fn peer_input_rejections_note(
    peers_root: &Path,
    session: &SessionKey,
) -> Option<String> {
    if session.topic().is_some_and(|topic| {
        topic.starts_with("peer-") || topic.starts_with(PEER_CONTEXT_TOPIC_PREFIX)
    }) {
        return None;
    }
    let read_dir = std::fs::read_dir(peers_root).ok()?;
    let mut dirs: Vec<_> = read_dir.flatten().collect();
    dirs.sort_by_key(|entry| entry.file_name());
    let mut lines = Vec::new();
    let mut total = 0usize;
    for entry in dirs {
        let slug = entry.file_name().to_string_lossy().into_owned();
        let Some(dir) = staged_peer_dir(peers_root, &slug) else {
            continue;
        };
        if !peer_io::peer_regular_file_exists(&dir, INPUT_REJECTIONS_LEAF) {
            continue;
        }
        let originator =
            peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL);
        if originator.as_deref().map(str::trim) != Some(session.0.as_str()) {
            continue;
        }
        let Some(body) = peer_io::read_peer_file(
            &dir,
            INPUT_REJECTIONS_LEAF,
            peer_io::PEER_FILE_READ_CAP_LARGE,
        ) else {
            continue;
        };
        // Only complete lines: a line being appended is read next time.
        let rows: Vec<&str> = body
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .collect();
        let told = peer_io::read_peer_file(
            &dir,
            INPUT_REJECTIONS_CURSOR_LEAF,
            peer_io::PEER_FILE_READ_CAP_SMALL,
        )
        .and_then(|cursor| cursor.trim().parse::<usize>().ok())
        .unwrap_or(0);
        if told >= rows.len() {
            continue;
        }
        let name = peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| slug.clone());
        for row in &rows[told..] {
            let Ok(row) = serde_json::from_str::<Value>(row) else {
                continue;
            };
            total += 1;
            if lines.len() >= INPUT_REJECTIONS_NOTE_MAX {
                continue;
            }
            let rejection = PeerInputRejection {
                reason: row["reason"]
                    .as_str()
                    .and_then(PeerInputRejectReason::parse)
                    .unwrap_or(PeerInputRejectReason::Other),
                message: row["message"].as_str().map(ToOwned::to_owned),
            };
            let addr = if name == slug {
                slug.clone()
            } else {
                format!("{name} ({slug})")
            };
            lines.push(format!("- {addr}: {}", rejection.describe()));
        }
        if let Err(error) = peer_io::write_peer_file_atomic(
            &dir,
            INPUT_REJECTIONS_CURSOR_LEAF,
            &rows.len().to_string(),
        ) {
            tracing::warn!(slug, ?error, "failed to record the reported input refusals");
        }
    }
    if lines.is_empty() {
        return None;
    }
    if total > lines.len() {
        lines.push(format!("- +{} more", total - lines.len()));
    }
    Some(format!(
        "[peer input rejected: the app refused these peer_send_input messages after the call \
         returned, so the peer did not act on them]\n{}",
        lines.join("\n")
    ))
}

/// The connection that holds the peer's route, if any.
pub(crate) fn host_route_connection(peers_root: &Path, slug: &str) -> Option<u64> {
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&route_key(peers_root, slug))
        .map(|route| route.connection)
}

/// Drop every route held by `connection` (it closed), end every call in
/// flight to it at once (as an unknown outcome unless it only read), and
/// forget the session tool sets it registered.
pub(crate) fn drop_routes_for_connection(connection: u64) {
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, route| route.connection != connection);
    for call in HUB
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
    {
        if call.connection == connection {
            call.cancel.fire("host_gone");
        }
    }
    SESSION_SETS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, registered| registered.connection != connection);
}

/// Drop the peer `slug`'s route at its host's request (`peer/tools/unregister`):
/// its calls in flight end `host_gone` and later input is refused as "not
/// connected", as if its connection had closed. Whether a route was held.
pub(crate) fn unregister_peer_route(peers_root: &Path, slug: &str) -> bool {
    let key = route_key(peers_root, slug);
    let send = HUB
        .routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .map(|route| route.send.clone());
    match send {
        Some(send) => {
            drop_route_if(&key, &send);
            true
        }
        None => false,
    }
}

/// Drop the route if it is still `send` (its connection closed).
///
/// Like a closed connection ([`drop_routes_for_connection`]), every call in
/// flight to that route's connection ends at once (`host_gone`) instead of
/// waiting out its timeout.
fn drop_route_if(key: &str, send: &HostSend) {
    let dropped = {
        let mut routes = HUB.routes.lock().unwrap_or_else(|p| p.into_inner());
        if routes
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(&current.send, send))
        {
            routes.remove(key).map(|route| route.connection)
        } else {
            None
        }
    };
    let Some(connection) = dropped else {
        return;
    };
    for call in HUB
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
    {
        if call.meta.route_key == key && call.connection == connection {
            call.cancel.fire("host_gone");
        }
    }
}

/// Why `peer/tool/result` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompleteError {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

/// A small insertion-ordered map that evicts its oldest entry when full.
pub(crate) struct BoundedMap<V = String> {
    order: std::collections::VecDeque<String>,
    map: HashMap<String, V>,
}

impl<V> BoundedMap<V> {
    const MAX: usize = 4_096;

    pub(crate) fn new() -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, key: String, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
            while self.order.len() > Self::MAX {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                }
            }
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, key: &str) {
        self.map.remove(key);
        self.order.retain(|k| k != key);
    }
}

/// Approvals raised by host-routed calls: approval id → the peer's route key.
static HOST_APPROVALS: LazyLock<Mutex<BoundedMap>> =
    LazyLock::new(|| Mutex::new(BoundedMap::new()));

/// The host-owned peer slug a `peer-<slug>` / `peerctx-<slug>.<id>` session
/// belongs to (syntax only).
pub(crate) fn host_peer_slug_of(session: &SessionKey) -> Option<&str> {
    let topic = session.topic()?;
    let slug = match topic.strip_prefix(PEER_CONTEXT_TOPIC_PREFIX) {
        Some(_) => parse_context_topic(topic)?.0,
        None => topic.strip_prefix("peer-")?,
    };
    peer_slug_is_safe(slug).then_some(slug)
}

/// Record that approval `approval_id` was raised by a host-routed call of the
/// set whose route is `route_key`: only that host connection may see or
/// answer it.
pub(crate) fn register_host_approval(approval_id: &str, route_key: String) {
    HOST_APPROVALS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(approval_id.to_owned(), route_key);
}

/// Whether `connection` may see the `approval/requested` event `event` (live,
/// on replay, in pending lists and hydrate). A host-routed call's approval
/// (`approval_kind: "host_tool"`, which carries the exact arguments) is
/// visible only to its host connection, and to nobody when the kernel no
/// longer knows its host (after a restart or an eviction). Every other
/// approval is decided by [`host_approval_visible`].
pub(crate) fn host_approval_event_visible(
    event: &octos_core::ui_protocol::ApprovalRequestedEvent,
    connection: u64,
) -> bool {
    let key = HOST_APPROVALS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&event.approval_id.0.to_string())
        .cloned();
    match key {
        Some(key) => route_connection_by_key(&key) == Some(connection),
        None => {
            event.approval_kind.as_deref()
                != Some(octos_core::ui_protocol::approval_kinds::HOST_TOOL)
        }
    }
}

/// Whether `connection` may see or answer approval `approval_id`: any
/// connection for an ordinary approval; only the peer's current host
/// connection for a host-routed one.
pub(crate) fn host_approval_visible(approval_id: &str, connection: u64) -> bool {
    let Some(key) = HOST_APPROVALS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(approval_id)
        .cloned()
    else {
        return true;
    };
    route_connection_by_key(&key) == Some(connection)
}

/// Whether `connection` may answer a host-routed call's approval that the
/// connection `raised_on` raised for the peer route `route_key`: only that
/// connection or the peer's current host connection. Read from the approval
/// entry itself, so it never fails open.
pub(crate) fn host_approval_answerable(
    route_key: &str,
    raised_on: Option<u64>,
    connection: u64,
) -> bool {
    raised_on == Some(connection) || route_connection_by_key(route_key) == Some(connection)
}

fn route_connection_by_key(key: &str) -> Option<u64> {
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(key)
        .map(|route| route.connection)
}

/// Whether a turn of `session` driven by `turn_connection` (`None` for a
/// kernel-internal continuation) may see the app's private context — its
/// memory namespace, its workspace and instructions. For a host-owned app
/// peer's session or request context only the peer's host connection (the
/// one holding its route) may; any other turn, and every continuation, gets
/// neither the app's context nor the profile's. Every other session: `true`.
pub(crate) fn app_context_allowed(
    peers_root: &Path,
    session: &SessionKey,
    turn_connection: Option<u64>,
) -> bool {
    let is_context = session
        .topic()
        .is_some_and(|t| t.starts_with(PEER_CONTEXT_TOPIC_PREFIX));
    let Some(slug) = host_peer_slug_of(session) else {
        // A malformed context topic never gets app context.
        return !is_context;
    };
    if !is_context && !super::app_binding::peer_is_host_owned(peers_root, slug) {
        return true;
    }
    turn_connection.is_some()
        && session_is_on_originator_base(peers_root, slug, session)
        && host_route_connection(peers_root, slug) == turn_connection
}

/// The controller of a `peer-`/`peerctx-` session, derived from what is on
/// disk, so it holds from the first call after a kernel restart: for a
/// session of a host-owned peer with a registered (or unreadable) tool set,
/// on the peer originator's base key, `Some(host connection)` (`Some(None)`
/// while no host is connected). `None` for any other session.
pub(crate) fn host_peer_session_controller(
    peers_root: &Path,
    session: &SessionKey,
) -> Option<Option<u64>> {
    let slug = host_peer_slug_of(session)?;
    if !super::app_binding::peer_is_host_owned(peers_root, slug)
        || matches!(read_tool_set(peers_root, slug), StoredToolSet::None)
        || !session_is_on_originator_base(peers_root, slug, session)
    {
        return None;
    }
    Some(host_route_connection(peers_root, slug))
}

/// A `peer/tool/result` from the host.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostReply {
    /// The call's final result.
    Final(HostToolCallOutcome),
    /// The app is asking the person; keep waiting (up to the approval TTL).
    AwaitingConfirmation,
}

/// Result of `peer/tool/result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompleteCall {
    Accepted,
    /// Accepted, but the data exceeded the cap: the model gets an error.
    TooLarge {
        bytes: usize,
        max: usize,
    },
    /// The acknowledgement extended the wait.
    Acknowledged,
}

/// Append one audit row to the peer's `tool_audit.jsonl`, unless the file
/// has reached its cap (then one `audit_full` marker is kept at the end).
pub(crate) fn append_audit(peers_root: &Path, slug: &str, row: &Value) {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return;
    };
    let size = std::fs::symlink_metadata(dir.join(TOOL_AUDIT_LEAF))
        .map(|m| m.len())
        .unwrap_or(0);
    let line = if size >= AUDIT_MAX_BYTES {
        return;
    } else if size + 1_024 >= AUDIT_MAX_BYTES {
        json!({ "ts": chrono::Utc::now().to_rfc3339(), "audit_full": true }).to_string()
    } else {
        row.to_string()
    };
    if let Err(error) = peer_io::append_peer_line(&dir, TOOL_AUDIT_LEAF, &format!("{line}\n")) {
        tracing::warn!(slug, %error, "failed to append the peer tool audit row");
    }
}

/// Upper bound of `tool_audit.jsonl`; the host owns rotation.
pub(crate) const AUDIT_MAX_BYTES: u64 = 16 * 1024 * 1024;

fn late_result_row(meta: &CallMeta, call_id: &str, reply: &HostReply) -> Value {
    let outcome = match reply {
        HostReply::Final(HostToolCallOutcome::Ok(_)) => "ok".to_owned(),
        HostReply::Final(HostToolCallOutcome::Error { kind, .. }) => format!("error:{kind}"),
        HostReply::AwaitingConfirmation => "awaiting_confirmation".to_owned(),
    };
    json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "peer": meta.host.peer(),
        "app": meta.app,
        "context_id": meta.context_id,
        "session_id": meta.session_id,
        "turn_id": meta.turn_id,
        "tools_version": meta.version,
        "tool": meta.tool,
        "tool_call_id": meta.tool_call_id,
        "call_id": call_id,
        "risk": meta.risk,
        "decision": "late_result",
        "outcome": outcome,
    })
}

/// Complete (or acknowledge) a pending call of the peer at
/// `peers_root`/`slug`. A result for a call the kernel stopped waiting for is
/// refused and audited as `late_result`.
pub(crate) fn complete_host_call(
    peers_root: &Path,
    host: &ToolHost,
    call_id: &str,
    from_connection: u64,
    reply: HostReply,
) -> Result<CompleteCall, CompleteError> {
    let key = host.route_key(peers_root);
    let owner = match host {
        ToolHost::Peer(slug) => format!("peer '{slug}'"),
        ToolHost::Session(session) => format!("session '{}'", session.0),
    };
    let not_found = |message: String| CompleteError {
        kind: "peer_tool_call_not_found",
        message,
    };
    let mut pending = HUB.pending.lock().unwrap_or_else(|p| p.into_inner());
    let owned = pending
        .get(call_id)
        .is_some_and(|call| call.meta.route_key == key);
    if !owned {
        drop(pending);
        let finished = HUB
            .finished
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(call_id)
            .filter(|(meta, _)| meta.route_key == key)
            .map(|(meta, _)| meta.clone());
        if let Some(meta) = finished {
            host.audit(peers_root, &late_result_row(&meta, call_id, &reply));
            return Err(not_found(format!(
                "call '{call_id}' already ended (timed out or cancelled); the late result \
                 was recorded but not given to the model"
            )));
        }
        return Err(not_found(format!(
            "no pending call '{call_id}' for {owner} (finished, timed out or cancelled)"
        )));
    }
    if pending
        .get(call_id)
        .is_some_and(|call| call.connection != from_connection)
    {
        return Err(CompleteError {
            kind: "peer_tool_result_wrong_connection",
            message: format!(
                "call '{call_id}' was sent to another connection; only that connection may answer it"
            ),
        });
    }
    let outcome = match reply {
        HostReply::AwaitingConfirmation => {
            let call = pending.get(call_id).expect("checked above");
            if !call.gated {
                return Err(CompleteError {
                    kind: "peer_tool_ack_not_gated",
                    message: format!(
                        "call '{call_id}' is not destructive or outward; only such a call may \
                         be acknowledged as awaiting confirmation"
                    ),
                });
            }
            call.ack.notify_one();
            return Ok(CompleteCall::Acknowledged);
        }
        HostReply::Final(outcome) => outcome,
    };
    // Remove AND deliver under the pending lock: a waiter whose deadline
    // fires concurrently either finds the call gone with the outcome already
    // in its channel, or removes it first (and this result is refused as
    // late). Never "removed but not yet delivered".
    let call = pending.remove(call_id).expect("checked above");
    #[cfg(test)]
    test_hooks::between_remove_and_deliver(host.peer().unwrap_or_default());
    let (outcome, status) = match outcome {
        HostToolCallOutcome::Ok(data) => {
            let bytes = serde_json::to_string(&data).map(|s| s.len()).unwrap_or(0);
            if bytes > call.max_result_bytes {
                (
                    HostToolCallOutcome::Error {
                        kind: "result_too_large".into(),
                        message: format!(
                            "the app returned {bytes} bytes (max {})",
                            call.max_result_bytes
                        ),
                    },
                    CompleteCall::TooLarge {
                        bytes,
                        max: call.max_result_bytes,
                    },
                )
            } else {
                (HostToolCallOutcome::Ok(data), CompleteCall::Accepted)
            }
        }
        error => (error, CompleteCall::Accepted),
    };
    let _ = call.tx.send(outcome);
    drop(pending);
    Ok(status)
}

/// Removes a pending call when the wait ends for any reason; if the kernel
/// gave up on it (timeout or turn interrupt), tells the host to stop it and
/// remembers it so a late result is audited.
struct PendingGuard {
    call_id: String,
    send: HostSend,
    reason: &'static str,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let removed = HUB
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.call_id);
        let Some(call) = removed else {
            return;
        };
        // The host may be acting on it: an interrupted or timed-out non-read
        // call must not be resent as if it never happened.
        if call.meta.risk != "read" {
            HUB.unknown.lock().unwrap_or_else(|p| p.into_inner()).mark(
                unknown_key(
                    &call.meta.route_key,
                    &call.meta.tool,
                    &call.meta.args_digest,
                ),
                UNKNOWN_RETENTION,
            );
        }
        {
            let mut finished = HUB.finished.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            finished.retain(|_, (_, at)| now.duration_since(*at) < FINISHED_RETENTION);
            if finished.len() >= FINISHED_MAX {
                if let Some(oldest) = finished
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(id, _)| id.clone())
                {
                    finished.remove(&oldest);
                }
            }
            finished.insert(self.call_id.clone(), (call.meta, now));
        }
        let _ = (self.send)(
            PEER_TOOL_CANCEL_NOTIFICATION,
            json!({ "call_id": self.call_id, "reason": self.reason }),
        );
    }
}

/// The per-turn router of one host peer session.
pub(crate) struct TurnHostToolRouter {
    pub(crate) peers_root: PathBuf,
    pub(crate) host: ToolHost,
    pub(crate) context_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) turn_id: String,
    pub(crate) version: u64,
    pub(crate) call_timeout: Duration,
    /// How long a call may wait on the person (the app's confirmation sheet).
    pub(crate) approval_ttl: Duration,
    pub(crate) max_result_bytes: usize,
}

impl TurnHostToolRouter {
    fn unknown_key(&self, tool: &str, args_digest: &str) -> String {
        unknown_key(&self.host.route_key(&self.peers_root), tool, args_digest)
    }

    fn error(kind: &str, message: impl Into<String>) -> HostToolCallOutcome {
        HostToolCallOutcome::Error {
            kind: kind.to_owned(),
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
impl HostToolRouter for TurnHostToolRouter {
    fn claim_occurrence(&self, tool_call_id: &str, args_digest: &str) -> OccurrenceClaim {
        // The `peer_send_input` occurrence shape (calling session, turn,
        // provider tool-call id) plus the argument digest, scoped to this
        // profile's peers root.
        let key = format!(
            "{}\u{0}{}/{}/{tool_call_id}/{args_digest}",
            self.peers_root.display(),
            self.session_id.0,
            self.turn_id
        );
        match HUB
            .occurrences
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .claim(key, OCCURRENCE_RETENTION)
        {
            Claim::Claimed => OccurrenceClaim::Claimed,
            Claim::AlreadyClaimed => OccurrenceClaim::Duplicate,
            Claim::Full => OccurrenceClaim::Busy,
        }
    }

    fn outcome_unknown_before(&self, tool: &str, args_digest: &str) -> bool {
        HUB.unknown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&self.unknown_key(tool, args_digest), UNKNOWN_RETENTION)
    }

    fn mark_outcome_unknown(&self, tool: &str, args_digest: &str) {
        HUB.unknown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mark(self.unknown_key(tool, args_digest), UNKNOWN_RETENTION);
    }

    fn clear_outcome_unknown(&self, tool: &str, args_digest: &str) {
        HUB.unknown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.unknown_key(tool, args_digest));
    }

    async fn call(&self, call: HostToolCall) -> HostToolCallOutcome {
        // #2500: an app whose peer budget is spent (or unreadable) cannot keep
        // working through its tools mid-turn, from any of its sessions.
        if let ToolHost::Peer(slug) = &self.host {
            match super::peer_token_budget_status(&self.peers_root, slug) {
                Ok(Some(status)) if status.used >= status.limit => {
                    return Self::error(
                        "budget_exhausted",
                        format!(
                            "peer '{}' token budget exhausted ({} used / {} limit)",
                            slug, status.used, status.limit
                        ),
                    );
                }
                Err(message) => return Self::error("budget_unavailable", message),
                Ok(_) => {}
            }
        }
        let key = self.host.route_key(&self.peers_root);
        let Some((send, connection)) = HUB
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .map(|route| (route.send.clone(), route.connection))
        else {
            return Self::error(
                "host_unavailable",
                "the app's host is not connected (it must register its tools on a live connection)",
            );
        };
        let call_id = format!("ptc-{}", uuid::Uuid::new_v4().simple());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ack = Arc::new(tokio::sync::Notify::new());
        let cancel = Arc::new(CallCancel::default());
        {
            let mut pending = HUB.pending.lock().unwrap_or_else(|p| p.into_inner());
            // Checked under the lock `cancel_host_calls_for_turn` marks it
            // under: either the interrupt sees this call pending and ends it,
            // or this call sees the interrupt and is never sent.
            if HUB
                .interrupted_turns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(
                    &interrupted_turn_key(&self.session_id, &self.turn_id),
                    INTERRUPTED_RETENTION,
                )
            {
                return Self::error(
                    "cancelled",
                    "the turn was interrupted; the call was not sent to the app",
                );
            }
            if pending.values().filter(|p| p.meta.route_key == key).count()
                >= MAX_PENDING_CALLS_PER_PEER
            {
                return Self::error(
                    "host_busy",
                    format!("{MAX_PENDING_CALLS_PER_PEER} calls are already in flight"),
                );
            }
            pending.insert(
                call_id.clone(),
                PendingCall {
                    meta: CallMeta {
                        route_key: key.clone(),
                        host: self.host.clone(),
                        app: call.app.clone(),
                        context_id: self.context_id.clone(),
                        session_id: self.session_id.clone(),
                        turn_id: self.turn_id.clone(),
                        version: self.version,
                        tool: call.name.clone(),
                        tool_call_id: call.tool_call_id.clone(),
                        risk: call.risk.as_str(),
                        args_digest: call.args_digest.clone(),
                    },
                    max_result_bytes: self.max_result_bytes,
                    tx,
                    ack: ack.clone(),
                    gated: call.gated,
                    connection,
                    cancel: cancel.clone(),
                },
            );
        }
        // Dropped on every exit: a turn interrupt drops this future and the
        // guard cancels the call with the host.
        let mut guard = PendingGuard {
            call_id: call_id.clone(),
            send: send.clone(),
            reason: "cancelled",
        };
        // A call the app must confirm with the person waits as long as an
        // approval would; any other call waits `call_timeout`, extended to
        // the approval TTL when the host acknowledges it is asking the person.
        let started = tokio::time::Instant::now();
        let mut deadline = started
            + if call.confirm_required {
                self.approval_ttl.max(self.call_timeout)
            } else {
                self.call_timeout
            };
        let delivered = send(
            PEER_TOOL_CALL_NOTIFICATION,
            json!({
                "peer": self.host.peer(),
                "session_id": self.session_id,
                "context_id": self.context_id,
                "turn_id": self.turn_id,
                "call_id": call_id,
                "tool_call_id": call.tool_call_id,
                "args_digest": call.args_digest,
                "name": call.name,
                "app": call.app,
                "caller": {
                    "kind": self.host.caller_kind(),
                    "peer": self.host.peer(),
                    "session_id": self.session_id,
                    "context_id": self.context_id,
                    "turn_id": self.turn_id,
                },
                "args": call.args,
                "risk": call.risk.as_str(),
                "confirm_required": call.confirm_required,
                "timeout_ms": deadline.duration_since(started).as_millis() as u64,
                "tools_version": self.version,
            }),
        );
        if !delivered {
            HUB.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&call_id);
            drop_route_if(&key, &send);
            return Self::error("host_unavailable", "the app's host connection is closed");
        }
        loop {
            tokio::select! {
                result = &mut rx => {
                    return match result {
                        Ok(outcome) => outcome,
                        Err(_) => Self::error(
                            "cancelled",
                            "the call was dropped before the app answered",
                        ),
                    };
                }
                _ = ack.notified(), if call.gated => {
                    deadline = deadline.max(started + self.approval_ttl);
                }
                _ = tokio::time::sleep_until(deadline) => break,
                _ = cancel.notify.notified() => {
                    // Interrupted turn or closed host connection: stop now.
                    let reason = cancel.reason();
                    // The host is told `cancelled` either way (a closed
                    // connection will not hear it).
                    guard.reason = "cancelled";
                    drop(guard);
                    if let Ok(outcome) = rx.try_recv() {
                        return outcome;
                    }
                    let (kind, what) = if reason == "host_gone" {
                        ("host_unavailable", "the app's host connection closed")
                    } else {
                        ("cancelled", "the turn was interrupted")
                    };
                    return if call.risk == HostToolRisk::Read {
                        Self::error(kind, format!("{what} before the app answered"))
                    } else {
                        Self::error(
                            "outcome_unknown",
                            format!(
                                "{what} while the app was working on this call, so it is \
                                 unknown whether it did this. Do not retry this call: check \
                                 the result with a read tool or ask the person."
                            ),
                        )
                    };
                }
            }
        }
        // Take the call out of the pending set FIRST (under the same lock
        // `peer/tool/result` completes it under), then look for a result: one
        // that landed before the removal is delivered, not reported unknown;
        // one that arrives after it is refused and audited as late.
        guard.reason = "timeout";
        drop(guard);
        if let Ok(outcome) = rx.try_recv() {
            return outcome;
        }
        let waited = deadline.duration_since(started).as_millis();
        if call.risk == HostToolRisk::Read {
            Self::error(
                "timeout",
                format!("the app did not answer within {waited} ms"),
            )
        } else {
            Self::error(
                "outcome_unknown",
                format!(
                    "the app did not answer within {waited} ms, so it is unknown whether it \
                     did this. Do not retry this call: check the result with a read tool or \
                     ask the person."
                ),
            )
        }
    }

    fn record(&self, audit: HostToolAudit) {
        let row = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "peer": self.host.peer(),
            "context_id": self.context_id,
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "tools_version": self.version,
            "tool": audit.tool,
            "app": audit.app,
            "tool_call_id": audit.tool_call_id,
            "risk": audit.risk,
            "decision": audit.decision,
            "outcome": audit.outcome,
            "duration_ms": audit.duration_ms,
            "args_bytes": audit.args_bytes,
            "result_bytes": audit.result_bytes,
        });
        self.host.audit(&self.peers_root, &row);
    }

    fn call_timeout(&self) -> Duration {
        self.call_timeout
    }
}

/// Test-only pause injected between taking a call out of the pending set
/// and delivering its result, to prove no waiter observes the gap.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::Mutex;

    static DELAY: Mutex<Option<(String, std::time::Duration)>> = Mutex::new(None);

    pub(crate) fn delay_delivery_for(slug: &str, delay: std::time::Duration) {
        *DELAY.lock().unwrap() = Some((slug.to_owned(), delay));
    }

    pub(super) fn between_remove_and_deliver(slug: &str) {
        let delay = DELAY
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(s, _)| s == slug)
            .map(|(_, d)| *d);
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
    }
}

#[cfg(test)]
pub(crate) fn pending_calls_for(peers_root: &Path, slug: &str) -> Vec<String> {
    let key = route_key(peers_root, slug);
    HUB.pending
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, call)| call.meta.route_key == key)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_always_record_a_new_mark_when_the_set_is_full() {
        // #2616 review nit: a full interrupted-turn set must never lose the
        // interrupt being recorded (that would let its calls through); it
        // forgets the oldest one instead, and says so.
        let retention = Duration::from_secs(3_600);
        let mut set = BoundedClaims::default();
        for i in 0..BoundedClaims::MAX {
            assert!(!set.mark(format!("t{i}"), retention));
        }
        assert!(set.mark("newest".into(), retention), "reports the eviction");
        assert!(set.contains("newest", retention));
        assert!(!set.contains("t0", retention));
        assert!(set.contains("t1", retention));
    }

    fn tool(name: &str, risk: &str) -> ToolInput {
        serde_json::from_value(json!({
            "name": name,
            "description": format!("{name} tool"),
            "input_schema": {"type": "object"},
            "risk": risk,
        }))
        .unwrap()
    }

    #[test]
    fn should_validate_names_schemas_and_collisions() {
        assert!(validate_app_tool_name("news.list").is_ok());
        assert!(validate_app_tool_name("news.topics.get").is_ok());
        for bad in [
            "news",
            "News.list",
            "news..list",
            "news.list-all",
            "a.b.c.d.e",
        ] {
            assert!(validate_app_tool_name(bad).is_err(), "{bad}");
        }
        let set = build_tool_set(
            vec![tool("news.list", "read")],
            Some(vec!["deep_search".into(), "deep_search".into()]),
            ToolSetOptions::default(),
        )
        .unwrap();
        assert_eq!(set.tools[0].model_name, "news_list");
        assert_eq!(set.generic_tools, Some(vec!["deep_search".to_owned()]));
        assert_eq!(set.call_timeout_ms, DEFAULT_CALL_TIMEOUT_MS);

        let collision = build_tool_set(
            vec![tool("news.a_b", "read"), tool("news_a.b", "read")],
            None,
            ToolSetOptions::default(),
        );
        assert!(collision.unwrap_err().contains("collides"));

        let mut not_object = tool("news.list", "read");
        not_object.input_schema = json!({"type": "string"});
        assert!(build_tool_set(vec![not_object], None, Default::default()).is_err());
        assert!(build_tool_set(vec![], Some(vec!["rm -rf".into()]), Default::default()).is_err());
    }

    #[test]
    fn should_accept_a_tools_json_entry_and_refuse_unknown_fields() {
        let tools: Vec<ToolInput> = serde_json::from_value(json!([
            {"name": "rinx.read_thread", "description": "Read a thread.",
             "input_schema": {"type": "object"}, "risk": "read",
             "background": true, "shareable": false},
            {"name": "rinx.send_message", "description": "Send a message.",
             "input_schema": {"type": "object", "required": ["text"]},
             "output_schema": {"type": "object"},
             "risk": "destructive", "confirm": "app"}
        ]))
        .unwrap();
        let set = build_tool_set(tools, None, Default::default()).unwrap();
        assert_eq!(set.tools[0].confirm, HostToolConfirm::Host, "default");
        assert!(set.tools[0].background);
        assert_eq!(set.tools[1].confirm, HostToolConfirm::App);
        assert_eq!(set.tools[1].risk, HostToolRisk::Destructive);

        let typo = serde_json::from_value::<ToolInput>(json!({
            "name": "rinx.x", "description": "x", "input_schema": {"type": "object"},
            "risk": "read", "confirmed": "app"
        }));
        assert!(typo.is_err(), "a misspelled field is refused, not ignored");
        let bad_confirm = serde_json::from_value::<ToolInput>(json!({
            "name": "rinx.x", "description": "x", "input_schema": {"type": "object"},
            "risk": "read", "confirm": "nobody"
        }));
        assert!(bad_confirm.is_err());
    }

    /// A result that lands at the deadline is delivered, never reported as
    /// `outcome_unknown` while the host was told `accepted`. Paused time
    /// makes the deadline and the result ready at the same poll, so the
    /// `select!` takes either branch across iterations; both must deliver.
    #[tokio::test(start_paused = true)]
    async fn should_deliver_a_result_that_lands_at_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        for round in 0..40 {
            let slug = format!("race{round}");
            let (tx, rx) = std::sync::mpsc::channel::<(String, Value)>();
            let tx = std::sync::Mutex::new(tx);
            set_host_route(
                tmp.path(),
                &slug,
                7_000 + round,
                Arc::new(move |method, params| {
                    tx.lock().unwrap().send((method.to_owned(), params)).is_ok()
                }),
            );
            let router = TurnHostToolRouter {
                peers_root: tmp.path().to_path_buf(),
                host: ToolHost::Peer(slug.clone()),
                context_id: None,
                session_id: SessionKey(format!("octos:api:host#peer-{slug}")),
                turn_id: "t".into(),
                version: 1,
                call_timeout: Duration::from_millis(100),
                approval_ttl: Duration::from_secs(60),
                max_result_bytes: 1024,
            };
            let task = tokio::spawn(async move {
                router
                    .call(HostToolCall {
                        tool_call_id: "c1".into(),
                        name: "news.topics_set".into(),
                        app: "news".into(),
                        args: json!({}),
                        risk: HostToolRisk::Act,
                        confirm_required: false,
                        gated: false,
                        args_digest: "sha256:x".into(),
                    })
                    .await
            });
            let call_id = loop {
                tokio::task::yield_now().await;
                if let Ok((method, params)) = rx.try_recv() {
                    assert_eq!(method, PEER_TOOL_CALL_NOTIFICATION);
                    break params["call_id"].as_str().unwrap().to_owned();
                }
            };
            let accepted = complete_host_call(
                tmp.path(),
                &ToolHost::Peer(slug.clone()),
                &call_id,
                7_000 + round,
                HostReply::Final(HostToolCallOutcome::Ok(json!({"n": round}))),
            )
            .expect("accepted");
            assert_eq!(accepted, CompleteCall::Accepted);
            tokio::time::advance(Duration::from_millis(500)).await;
            assert_eq!(
                task.await.unwrap(),
                HostToolCallOutcome::Ok(json!({"n": round})),
                "round {round}"
            );
            assert!(
                rx.try_iter()
                    .all(|(method, _)| method != PEER_TOOL_CANCEL_NOTIFICATION),
                "an answered call is never cancelled"
            );
        }
    }

    #[test]
    fn should_evict_only_expired_claims_and_refuse_when_full() {
        let mut claims = BoundedClaims::default();
        let retention = Duration::from_secs(3_600);
        for i in 0..BoundedClaims::MAX {
            assert_eq!(claims.claim(format!("k{i}"), retention), Claim::Claimed);
        }
        assert_eq!(claims.claim("k0".into(), retention), Claim::AlreadyClaimed);
        assert_eq!(claims.claim("new".into(), retention), Claim::Full);
        assert!(
            claims.contains("k0", retention),
            "a fresh claim is never evicted"
        );
        // Expired claims make room.
        assert_eq!(claims.claim("new".into(), Duration::ZERO), Claim::Claimed);
    }

    /// Deterministic: the host's result is taken out of the pending set just
    /// before the waiter's deadline and delivered after it (an injected
    /// pause). The waiter must still deliver it, not report
    /// `outcome_unknown`, and send no cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_deliver_a_result_taken_before_the_deadline_but_delivered_after_it() {
        let tmp = tempfile::tempdir().unwrap();
        let slug = "gap".to_owned();
        let (tx, rx) = std::sync::mpsc::channel::<(String, Value)>();
        let tx = std::sync::Mutex::new(tx);
        set_host_route(
            tmp.path(),
            &slug,
            9_001,
            Arc::new(move |method, params| {
                tx.lock().unwrap().send((method.to_owned(), params)).is_ok()
            }),
        );
        test_hooks::delay_delivery_for(&slug, Duration::from_millis(400));
        let router = TurnHostToolRouter {
            peers_root: tmp.path().to_path_buf(),
            host: ToolHost::Peer(slug.clone()),
            context_id: None,
            session_id: SessionKey("octos:api:host#peer-gap".into()),
            turn_id: "t".into(),
            version: 1,
            call_timeout: Duration::from_millis(200),
            approval_ttl: Duration::from_secs(60),
            max_result_bytes: 1024,
        };
        let task = tokio::spawn(async move {
            router
                .call(HostToolCall {
                    tool_call_id: "c1".into(),
                    name: "news.topics_set".into(),
                    app: "news".into(),
                    args: json!({}),
                    risk: HostToolRisk::Act,
                    confirm_required: false,
                    gated: false,
                    args_digest: "sha256:x".into(),
                })
                .await
        });
        let (_, params) = tokio::task::spawn_blocking(move || rx.recv().unwrap())
            .await
            .unwrap();
        let call_id = params["call_id"].as_str().unwrap().to_owned();
        let peers_root = tmp.path().to_path_buf();
        let completer = std::thread::spawn(move || {
            // Start completing at ~50 ms; the pause holds the delivery until
            // ~450 ms, well past the 200 ms deadline.
            std::thread::sleep(Duration::from_millis(50));
            complete_host_call(
                &peers_root,
                &ToolHost::Peer("gap".into()),
                &call_id,
                9_001,
                HostReply::Final(HostToolCallOutcome::Ok(json!({"done": true}))),
            )
        });
        let outcome = task.await.unwrap();
        assert_eq!(completer.join().unwrap(), Ok(CompleteCall::Accepted));
        assert_eq!(outcome, HostToolCallOutcome::Ok(json!({"done": true})));
    }
}
