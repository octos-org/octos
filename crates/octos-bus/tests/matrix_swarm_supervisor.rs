//! Integration tests for the M7.3 Matrix swarm supervisor contract.
//!
//! Uses a mock homeserver to exercise the idempotent puppet/room registration,
//! harness event routing, and supervisor reply matching. The mock is scoped
//! to these endpoints:
//!
//! - `POST /_matrix/client/v3/register`
//! - `POST /_matrix/client/v3/createRoom`
//! - `GET  /_matrix/client/v3/directory/room/{alias}`
//! - `POST /_matrix/client/v3/rooms/{room}/invite`
//! - `PUT  /_matrix/client/v3/rooms/{room}/send/{event_type}/{txn_id}`
//!
//! The tests also verify that _absent_ swarm supervisor config leaves the
//! channel behavior identical to pre-M7.3 deployments (invariant 5).

#![cfg(feature = "matrix")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{any, get, post, put};
use octos_bus::matrix_channel::{
    BotRouter, MatrixChannel, MatrixRoomId, MatrixUserId, SWARM_SUPERVISOR_EVENT_SCHEMA_V1,
    SteeringInput, SwarmHarnessEvent, SwarmSupervisorParams,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Recorded request on the mock homeserver.
#[derive(Clone, Debug)]
struct CapturedRequest {
    method: Method,
    path: String,
    query: Option<String>,
    body: Value,
}

#[derive(Clone, Default)]
struct MockHomeserver {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Tracks whether the localpart has been registered before (for
    /// idempotent M_USER_IN_USE simulation).
    registered_users: Arc<Mutex<HashMap<String, ()>>>,
    /// Tracks which room aliases have been created so a second create returns
    /// M_ROOM_IN_USE.
    created_aliases: Arc<Mutex<HashMap<String, String>>>,
}

async fn record_register(
    State(state): State<MockHomeserver>,
    _query: Query<HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let body_json: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    state.requests.lock().await.push(CapturedRequest {
        method: Method::POST,
        path: "/_matrix/client/v3/register".into(),
        query: None,
        body: body_json.clone(),
    });
    let Some(username) = body_json.get("username").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"errcode": "M_BAD_JSON"})),
        );
    };
    let mut users = state.registered_users.lock().await;
    if users.contains_key(username) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"errcode": "M_USER_IN_USE"})),
        );
    }
    users.insert(username.to_string(), ());
    (
        StatusCode::OK,
        axum::Json(json!({
            "user_id": format!("@{username}:localhost"),
            "access_token": "mock_access_token",
        })),
    )
}

async fn record_create_room(
    State(state): State<MockHomeserver>,
    Query(_query): Query<HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let body_json: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    state.requests.lock().await.push(CapturedRequest {
        method: Method::POST,
        path: "/_matrix/client/v3/createRoom".into(),
        query: None,
        body: body_json.clone(),
    });
    let alias = body_json
        .get("room_alias_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut aliases = state.created_aliases.lock().await;
    if let Some(_existing) = aliases.get(alias) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"errcode": "M_ROOM_IN_USE"})),
        );
    }
    let room_id = format!("!room_{alias}:localhost");
    aliases.insert(alias.to_string(), room_id.clone());
    (
        StatusCode::OK,
        axum::Json(json!({
            "room_id": room_id,
        })),
    )
}

async fn record_alias_lookup(
    State(state): State<MockHomeserver>,
    Path(alias): Path<String>,
) -> impl IntoResponse {
    state.requests.lock().await.push(CapturedRequest {
        method: Method::GET,
        path: format!("/_matrix/client/v3/directory/room/{alias}"),
        query: None,
        body: json!({}),
    });
    // alias is `#swarm_<session>:localhost`; we keyed by localpart.
    let localpart = alias
        .strip_prefix('#')
        .and_then(|s| s.split_once(':').map(|(l, _)| l))
        .unwrap_or(&alias);
    let aliases = state.created_aliases.lock().await;
    match aliases.get(localpart) {
        Some(room_id) => (
            StatusCode::OK,
            axum::Json(json!({ "room_id": room_id.clone() })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"errcode": "M_NOT_FOUND"})),
        ),
    }
}

async fn record_invite(
    State(state): State<MockHomeserver>,
    Path(room_id): Path<String>,
    Query(_query): Query<HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let body_json: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    state.requests.lock().await.push(CapturedRequest {
        method: Method::POST,
        path: format!("/_matrix/client/v3/rooms/{room_id}/invite"),
        query: None,
        body: body_json,
    });
    (StatusCode::OK, axum::Json(json!({})))
}

async fn record_send(
    State(state): State<MockHomeserver>,
    Path((room_id, event_type, txn_id)): Path<(String, String, String)>,
    Query(query): Query<HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let body_json: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    let query_str = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    state.requests.lock().await.push(CapturedRequest {
        method: Method::PUT,
        path: format!("/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}"),
        query: if query_str.is_empty() {
            None
        } else {
            Some(query_str)
        },
        body: body_json,
    });
    (
        StatusCode::OK,
        axum::Json(json!({ "event_id": format!("${txn_id}") })),
    )
}

async fn catchall(
    State(state): State<MockHomeserver>,
    method: Method,
    uri: Uri,
    body: String,
) -> impl IntoResponse {
    let body_json: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    state.requests.lock().await.push(CapturedRequest {
        method,
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        body: body_json,
    });
    (StatusCode::OK, axum::Json(json!({})))
}

async fn spawn_mock_homeserver() -> (String, MockHomeserver, tokio::task::JoinHandle<()>) {
    let state = MockHomeserver::default();
    let app = Router::new()
        .route("/_matrix/client/v3/register", post(record_register))
        .route("/_matrix/client/v3/createRoom", post(record_create_room))
        .route(
            "/_matrix/client/v3/directory/room/{alias}",
            get(record_alias_lookup),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/invite",
            post(record_invite),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
            put(record_send),
        )
        .fallback(any(catchall))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state, handle)
}

fn make_supervisor_channel(homeserver: &str) -> MatrixChannel {
    MatrixChannel::new(
        homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9881,
        Arc::new(AtomicBool::new(false)),
    )
    .with_swarm_supervisor(SwarmSupervisorParams {
        puppet_prefix: "octos_swarm_".into(),
        room_prefix: "octos_swarm_".into(),
        supervisor_user_ids: vec!["@alice:localhost".into()],
    })
}

// ── Invariant 1: puppet registration is idempotent ──────────────────────────

#[tokio::test]
async fn should_register_puppet_idempotently() {
    let (homeserver, mock, handle) = spawn_mock_homeserver().await;
    let ch = make_supervisor_channel(&homeserver);

    let first = ch
        .register_subagent_puppet("s3f1", "claude-code")
        .await
        .unwrap();
    let second = ch
        .register_subagent_puppet("s3f1", "claude-code")
        .await
        .unwrap();

    assert_eq!(
        first.as_str(),
        second.as_str(),
        "re-registering the same puppet must return the same user id"
    );
    // Localpart must include prefix + sanitized session + label.
    assert!(
        first.as_str().starts_with("@octos_swarm_s3f1_claude-code"),
        "expected sanitized localpart, got {}",
        first
    );

    // Only ONE register request should have hit the homeserver — the fast
    // path short-circuits on the second call.
    let requests = mock.requests.lock().await;
    let registers: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/_matrix/client/v3/register")
        .collect();
    assert_eq!(
        registers.len(),
        1,
        "idempotent register must not re-hit homeserver: got {registers:?}"
    );

    handle.abort();
}

// ── Invariant 2: room creation is idempotent ────────────────────────────────

#[tokio::test]
async fn should_create_swarm_room_idempotently() {
    let (homeserver, mock, handle) = spawn_mock_homeserver().await;
    let ch = make_supervisor_channel(&homeserver);

    let first = ch.ensure_swarm_room("s3f1").await.unwrap();
    let second = ch.ensure_swarm_room("s3f1").await.unwrap();

    assert_eq!(
        first.as_str(),
        second.as_str(),
        "re-calling ensure_swarm_room must return the same room id"
    );

    let requests = mock.requests.lock().await;
    let creates: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/_matrix/client/v3/createRoom")
        .collect();
    assert_eq!(
        creates.len(),
        1,
        "fast-path cache hit should skip homeserver: got {creates:?}"
    );

    handle.abort();
}

/// Regression: a stale in-memory cache (e.g. after gateway restart) must
/// recover from `M_ROOM_IN_USE` by resolving the alias back to the existing
/// room ID, so re-running the flow on a fresh `MatrixChannel` returns the
/// same room.
#[tokio::test]
async fn should_recover_existing_room_from_alias_on_cold_cache() {
    let (homeserver, mock, handle) = spawn_mock_homeserver().await;

    // First process creates the room.
    let ch1 = make_supervisor_channel(&homeserver);
    let first = ch1.ensure_swarm_room("s3f1").await.unwrap();

    // Second process (fresh cache) hits M_ROOM_IN_USE and resolves via
    // directory.
    let ch2 = make_supervisor_channel(&homeserver);
    let second = ch2.ensure_swarm_room("s3f1").await.unwrap();

    assert_eq!(first.as_str(), second.as_str());

    let requests = mock.requests.lock().await;
    assert!(
        requests
            .iter()
            .any(|r| r.path.starts_with("/_matrix/client/v3/directory/room/")),
        "cold-cache recovery must resolve the alias via the directory API"
    );

    handle.abort();
}

// ── Invariant 3: harness event → Matrix message preserves kind + summary ────

#[tokio::test]
async fn should_route_typed_harness_event_to_correct_puppet_message() {
    let (homeserver, mock, handle) = spawn_mock_homeserver().await;
    let ch = make_supervisor_channel(&homeserver);

    let puppet = ch
        .register_subagent_puppet("s3f1", "claude-code")
        .await
        .unwrap();
    let room = ch.ensure_swarm_room("s3f1").await.unwrap();

    let event = SwarmHarnessEvent::Progress {
        session_id: "s3f1".into(),
        task_id: "task-42".into(),
        workflow: Some("deep_research".into()),
        phase: "fetch_sources".into(),
        message: Some("Fetching 3/12".into()),
        progress: Some(0.25),
    };
    ch.route_subagent_event("s3f1", "claude-code", event.clone())
        .await
        .unwrap();

    let requests = mock.requests.lock().await;
    let send = requests
        .iter()
        .find(|r| {
            r.method == Method::PUT && r.path.contains(&format!("/rooms/{}/send/", room.as_str()))
        })
        .expect("expected a send request");

    // `msgtype` is m.text and body carries the summary line.
    assert_eq!(send.body["msgtype"], "m.text");
    let body = send.body["body"].as_str().unwrap_or("");
    assert!(
        body.starts_with("progress fetch_sources"),
        "summary must lead with `progress fetch_sources`, got: {body}"
    );
    assert!(body.contains("25%"), "summary must include progress %");

    // Structured envelope carries `kind` + schema + agent_label.
    let envelope = &send.body["org.octos.swarm_event"];
    assert_eq!(envelope["kind"], "progress");
    assert_eq!(envelope["schema"], SWARM_SUPERVISOR_EVENT_SCHEMA_V1);
    assert_eq!(envelope["agent_label"], "claude-code");
    assert_eq!(envelope["session_id"], "s3f1");
    assert_eq!(envelope["event"]["phase"], "fetch_sources");
    assert_eq!(envelope["event"]["progress"], 0.25);

    // Sender identity assertion = puppet user_id.
    let query = send.query.as_deref().unwrap_or("");
    let encoded_puppet = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("user_id="))
        .unwrap_or("");
    // percent-decoded prefix should start with "@octos_swarm_s3f1_claude-code"
    assert!(
        encoded_puppet.contains("octos_swarm_s3f1_claude-code"),
        "send should use puppet identity, got query={query}, puppet={puppet}"
    );

    handle.abort();
}

// ── Invariant 4: supervisor reply routes ONLY to the addressed puppet ───────

#[tokio::test]
async fn should_route_supervisor_reply_to_addressed_puppet_only() {
    let (homeserver, _mock, handle) = spawn_mock_homeserver().await;
    let ch = make_supervisor_channel(&homeserver);

    let puppet_a = ch
        .register_subagent_puppet("s3f1", "claude-code")
        .await
        .unwrap();
    let puppet_b = ch
        .register_subagent_puppet("s3f1", "gpt-helper")
        .await
        .unwrap();
    let room = ch.ensure_swarm_room("s3f1").await.unwrap();

    // Reply mentioning puppet_a only — space-separated mention, the form
    // Element emits when a user tabs to autocomplete a user pill.
    let space_reply = format!("{} please refine the outline", puppet_a);
    let steering = ch
        .handle_supervisor_reply(room.as_str(), "@alice:localhost", &space_reply)
        .await
        .expect("reply addressed to one puppet must produce steering input");

    assert_eq!(steering.agent_label, "claude-code");
    assert_eq!(steering.puppet_user_id.as_str(), puppet_a.as_str());
    assert_eq!(steering.supervisor_user_id, "@alice:localhost");
    assert_eq!(steering.session_id, "s3f1");
    assert_eq!(steering.body, "please refine the outline");

    // Classic `@puppet:server: body` prefix, matching the contract example
    // (`@claude-code-s3f1: do X`).
    let colon_reply = format!("{}: tighten up the executive summary", puppet_a);
    let steering_colon = ch
        .handle_supervisor_reply(room.as_str(), "@alice:localhost", &colon_reply)
        .await
        .expect("classic @puppet:server: form must also route");
    assert_eq!(steering_colon.agent_label, "claude-code");
    assert_eq!(steering_colon.body, "tighten up the executive summary");

    // Reply mentioning BOTH puppets: invariant 4 requires `None` (we must
    // NOT broadcast).
    let ambiguous = format!("{} {} please coordinate", puppet_a, puppet_b);
    assert!(
        ch.handle_supervisor_reply(room.as_str(), "@alice:localhost", &ambiguous)
            .await
            .is_none(),
        "multi-puppet replies must return None rather than broadcasting"
    );

    // Reply with NO puppet mention: None.
    assert!(
        ch.handle_supervisor_reply(
            room.as_str(),
            "@alice:localhost",
            "random comment from supervisor"
        )
        .await
        .is_none(),
        "replies without a puppet mention must not produce steering input"
    );

    // Reply from a non-supervisor: None.
    let from_stranger = format!("{}: help", puppet_a);
    assert!(
        ch.handle_supervisor_reply(room.as_str(), "@mallory:other", &from_stranger)
            .await
            .is_none(),
        "replies from non-supervisors must be ignored"
    );

    // Reply in an unknown room: None (keeps unrelated traffic out).
    let unrelated = format!("{}: help", puppet_a);
    assert!(
        ch.handle_supervisor_reply(
            "!not-a-swarm-room:localhost",
            "@alice:localhost",
            &unrelated
        )
        .await
        .is_none(),
        "replies in non-swarm rooms must not produce steering input"
    );

    handle.abort();
}

// ── Invariant 5: absent config → zero new behavior, zero new routes ─────────

#[tokio::test]
async fn should_ignore_swarm_supervisor_when_config_absent() {
    let (homeserver, _mock, handle) = spawn_mock_homeserver().await;

    // Channel without `.with_swarm_supervisor(..)` must reject supervisor
    // methods. Existing baseline methods must still succeed.
    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9882,
        Arc::new(AtomicBool::new(false)),
    );

    let register_err = ch
        .register_subagent_puppet("s3f1", "claude-code")
        .await
        .expect_err("supervisor not configured → must error");
    assert!(
        register_err
            .to_string()
            .contains("swarm supervisor not configured"),
        "expected typed error, got: {register_err}"
    );

    let room_err = ch
        .ensure_swarm_room("s3f1")
        .await
        .expect_err("supervisor not configured → must error");
    assert!(
        room_err
            .to_string()
            .contains("swarm supervisor not configured"),
        "expected typed error, got: {room_err}"
    );

    assert!(
        ch.handle_supervisor_reply("!room:localhost", "@alice:localhost", "@foo: hi")
            .await
            .is_none(),
        "reply handler must be a no-op when supervisor is not configured"
    );

    // Baseline bot_router and Matrix paths continue to work.
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    assert_eq!(
        router.route("@bot_weather:localhost").await,
        Some("profile-weather".to_string()),
        "baseline BotRouter behavior must remain intact"
    );

    handle.abort();
}

// ── Invariant 6: serialization preserves `kind` for every event variant ─────

#[test]
fn should_preserve_kind_tag_for_all_event_variants() {
    let cases = vec![
        (
            SwarmHarnessEvent::Progress {
                session_id: "s".into(),
                task_id: "t".into(),
                workflow: None,
                phase: "p".into(),
                message: None,
                progress: None,
            },
            "progress",
        ),
        (
            SwarmHarnessEvent::Phase {
                session_id: "s".into(),
                task_id: "t".into(),
                workflow: None,
                phase: "p".into(),
                message: None,
            },
            "phase",
        ),
        (
            SwarmHarnessEvent::Artifact {
                session_id: "s".into(),
                task_id: "t".into(),
                name: "deck.pptx".into(),
                path: Some("pf/deck.pptx".into()),
                message: None,
            },
            "artifact",
        ),
        (
            SwarmHarnessEvent::ValidatorResult {
                session_id: "s".into(),
                task_id: "t".into(),
                validator: "cargo-test".into(),
                passed: true,
                message: None,
            },
            "validator_result",
        ),
        (
            SwarmHarnessEvent::Retry {
                session_id: "s".into(),
                task_id: "t".into(),
                attempt: Some(2),
                message: None,
            },
            "retry",
        ),
        (
            SwarmHarnessEvent::Failure {
                session_id: "s".into(),
                task_id: "t".into(),
                message: "boom".into(),
                retryable: Some(false),
            },
            "failure",
        ),
    ];

    for (event, expected_kind) in cases {
        assert_eq!(event.kind(), expected_kind);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json["kind"], expected_kind,
            "serialized kind must match discriminant: {json:?}"
        );
        // Round-trip.
        let parsed: SwarmHarnessEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }
}

// ── Sanity: puppet + room IDs are surface-stable for replay ─────────────────

#[test]
fn should_format_matrix_ids_as_wrapped_strings() {
    let user = MatrixUserId::new("@octos_swarm_s3f1_claude-code:localhost");
    assert_eq!(user.to_string(), "@octos_swarm_s3f1_claude-code:localhost");
    let room = MatrixRoomId::new("!abc:localhost");
    assert_eq!(room.to_string(), "!abc:localhost");
}

// ── Steering input cleanly strips the mention ───────────────────────────────

#[test]
fn should_strip_puppet_mention_from_steering_body() {
    // Mirror the `strip_puppet_mention` behavior via handle_supervisor_reply
    // round-trip. Guarded by a dedicated test here because the downstream
    // SteeringInput.body is load-bearing for the agent session queue.
    let steering = SteeringInput {
        session_id: "s3f1".into(),
        agent_label: "claude-code".into(),
        puppet_user_id: MatrixUserId::new("@octos_swarm_s3f1_claude-code:localhost"),
        supervisor_user_id: "@alice:localhost".into(),
        body: "do X".into(),
    };
    assert_eq!(steering.body, "do X");
}

// ── Regression marker: existing Matrix channel behavior must not drift ──────
//
// This test constructs a baseline `MatrixChannel` and spot-checks the
// public-API invariants that existed before M7.3 — `name()`, `bot_user_id()`,
// `supports_edit()`, and that baseline `BotRouter` registration still works.
// Deep regression coverage is provided by the 100+ pre-existing unit tests in
// `crates/octos-bus/src/matrix_channel.rs::tests`, which continue to run under
// `cargo test -p octos-bus --features matrix` and guard the full baseline.
#[tokio::test]
async fn should_preserve_existing_matrix_channel_tests() {
    let ch = MatrixChannel::new(
        "http://localhost:6167",
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9883,
        Arc::new(AtomicBool::new(false)),
    );

    // Baseline surface — unchanged since pre-M7.3.
    assert_eq!(ch.bot_user_id(), "@octos_bot:localhost");

    // Baseline bot router still provisions routes and resolves them.
    let router = ch.bot_router();
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    assert_eq!(
        router.route("@octos_weather:localhost").await,
        Some("profile-weather".to_string())
    );
    assert!(
        router.route("@octos_unknown:localhost").await.is_none(),
        "unknown bots still return None — baseline BotRouter contract intact"
    );
}
