use super::*;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Uri};
use axum::routing::any;
use tokio::sync::Mutex;
use tower::ServiceExt;

fn make_channel() -> MatrixChannel {
    MatrixChannel::new(
        "http://localhost:6167",
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9880,
        Arc::new(AtomicBool::new(false)),
    )
}

fn make_test_state(inbound_tx: mpsc::Sender<InboundMessage>) -> AppserviceState {
    let mut registered = HashSet::new();
    registered.insert("@octos_bot:localhost".to_string());
    AppserviceState {
        inbound_tx,
        homeserver: "http://localhost:6167".to_string(),
        as_token: "test_as_token".to_string(),
        hs_token: "test_token".to_string(),
        bot_user_id: "@octos_bot:localhost".to_string(),
        server_name: "localhost".to_string(),
        user_prefix: "octos_".to_string(),
        http: reqwest::Client::new(),
        registered_users: Arc::new(RwLock::new(registered)),
        dedup: Arc::new(MessageDedup::new()),
        bot_router: Arc::new(BotRouter::new(None)),
        bot_manager: None,
        media_dir: Some(std::env::temp_dir().join("octos-matrix-test-media")),
        // Existing transaction tests exercise routing/enqueue without the
        // mention gate; enable it explicitly in the gate's own tests.
        mention_only: false,
        dm_member_cache: Arc::new(RwLock::new(HashMap::new())),
        dm_member_cache_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ignored_senders: HashSet::new(),
        // Cron slash commands are operator-gated; tests that exercise them
        // add their sender explicitly, everything else stays fail-closed.
        admin_allowed_senders: HashSet::new(),
    }
}

fn transaction_app(state: AppserviceState) -> Router {
    Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state)
}

async fn put_transaction(app: Router, txn_id: &str, body: Body) -> axum::response::Response {
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "/_matrix/app/v1/transactions/{txn_id}?access_token=test_token"
        ))
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    app.oneshot(request).await.unwrap()
}

// ── mention-only gate ────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
}

#[derive(Clone)]
struct MockHomeserverState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    status: StatusCode,
    response_body: Value,
}

async fn capture_homeserver_request(
    State(state): State<MockHomeserverState>,
    uri: Uri,
) -> impl IntoResponse {
    state.requests.lock().await.push(CapturedRequest {
        path: uri.path().to_string(),
    });
    (
        state.status,
        serde_json::to_string(&state.response_body).unwrap(),
    )
}

async fn spawn_mock_homeserver() -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_homeserver_with_response(StatusCode::OK, json!({"event_id":"$test_event"})).await
}

async fn spawn_mock_homeserver_with_response(
    status: StatusCode,
    response_body: Value,
) -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = MockHomeserverState {
        requests: requests.clone(),
        status,
        response_body,
    };
    let app = Router::new()
        .route(
            "/_matrix/client/v3/register",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/account/whoami",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/join",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/leave",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/typing/{user_id}",
            any(capture_homeserver_request),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/joined_members",
            any(capture_homeserver_request),
        )
        .route("/_matrix/media/v3/upload", any(capture_homeserver_request))
        .route(
            "/_matrix/media/v3/download/{server_name}/{media_id}",
            any(capture_homeserver_request),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{}", addr), requests, handle)
}

fn unused_local_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// #1546 — an operator-configured ignored sender's m.text is dropped before
// any handler sees it, breaking bot-to-bot reply loops.
#[tokio::test]
async fn test_handle_transaction_drops_ignored_senders_even_as_text() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let mut state = make_test_state(inbound_tx);
    state
        .ignored_senders
        .insert("@peer_bot:localhost".to_string());

    let app = transaction_app(state);

    // Two events from a peer bot: an m.notice AND an m.text. Neither may
    // reach the handler — the whole point of #1546 is that the loop used to
    // survive the m.notice guard via the m.text shape.
    let events = json!({
        "events": [
            {
                "type": "m.room.message",
                "sender": "@peer_bot:localhost",
                "room_id": "!room:localhost",
                "event_id": "$ignored_notice_1",
                "origin_server_ts": 1,
                "content": {"msgtype": "m.notice", "body": "bot notice"}
            },
            {
                "type": "m.room.message",
                "sender": "@peer_bot:localhost",
                "room_id": "!room:localhost",
                "event_id": "$ignored_text_1",
                "origin_server_ts": 2,
                "content": {"msgtype": "m.text", "body": "bot text that used to loop"}
            }
        ]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/ignored_txn")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test_token")
        .body(Body::from(events.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        inbound_rx.try_recv().is_err(),
        "ignored sender's messages must not reach any handler"
    );
}

// ── Token validation tests ───────────────────────────────────────────

// ── txn_id dedup test ────────────────────────────────────────────────

// ── Registered users test ────────────────────────────────────────────

// ── Axum handler integration tests ───────────────────────────────────

#[tokio::test]
async fn test_handle_transaction_bad_json_does_not_poison_txn_id() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let state = make_test_state(inbound_tx);

    let app = transaction_app(state);

    let bad_resp = put_transaction(app.clone(), "txn_retry", Body::from("{not-json")).await;
    assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);

    let good_body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room:localhost",
            "event_id": "$ev_retry",
            "content": {
                "msgtype": "m.text",
                "body": "retry should still deliver"
            }
        }]
    });
    let good_resp = put_transaction(app, "txn_retry", Body::from(good_body.to_string())).await;
    assert_eq!(good_resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(msg.content, "retry should still deliver");
}

#[tokio::test]
async fn media_download_redirect_does_not_leak_as_token() {
    use axum::response::Redirect;

    // "Attacker" origin: records any Authorization header it receives.
    let leaked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let attacker = Router::new().route(
        "/steal",
        axum::routing::get({
            let leaked = leaked.clone();
            move |headers: axum::http::HeaderMap| {
                let leaked = leaked.clone();
                async move {
                    if let Some(auth) = headers.get("authorization") {
                        leaked
                            .lock()
                            .await
                            .push(auth.to_str().unwrap_or("").to_string());
                    }
                    "stolen"
                }
            }
        }),
    );
    let attacker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let attacker_addr = attacker_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(attacker_listener, attacker).await.unwrap();
    });

    // "Homeserver": 302-redirects the media download to the attacker origin.
    let redirect_target = format!("http://{attacker_addr}/steal");
    let homeserver = Router::new().route(
        "/_matrix/media/v3/download/{server}/{media_id}",
        get(move || {
            let redirect_target = redirect_target.clone();
            async move { Redirect::temporary(&redirect_target) }
        }),
    );
    let homeserver_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let homeserver_addr = homeserver_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(homeserver_listener, homeserver).await.unwrap();
    });

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let ch = make_channel();
    let mut state = make_test_state(inbound_tx);
    state.http = ch.http.clone();
    state.homeserver = format!("http://{homeserver_addr}");

    let app = transaction_app(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room:localhost",
            "event_id": "$ev_redirect",
            "content": {
                "msgtype": "m.image",
                "body": "",
                "filename": "notes.txt",
                "url": "mxc://localhost/ABC123"
            }
        }]
    });
    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_redirect")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test_token")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The download fails instead of being re-issued at the redirect
    // target; the message degrades to text-only. reqwest strips
    // Authorization on cross-origin redirects by itself (the recorder
    // below pins that even for a following client), so the redirect
    // policy is the fail-loud layer, not the token's only defense.
    assert!(
        leaked.lock().await.is_empty(),
        "as_token must never reach the redirect target"
    );
    let msg = inbound_rx.try_recv().unwrap();
    assert!(
        msg.media.is_empty(),
        "redirected download must not be followed: {:?}",
        msg.media
    );
}

#[tokio::test]
async fn test_matrix_dedup_txn_id() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);

    let app = transaction_app(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room:localhost",
            "event_id": "$ev1",
            "content": {
                "msgtype": "m.text",
                "body": "first message"
            }
        }]
    });
    let body_str = serde_json::to_string(&body).unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_dedup?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(body_str.clone()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(inbound_rx.try_recv().is_ok());

    let req2 = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_dedup?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();
    assert_eq!(app.oneshot(req2).await.unwrap().status(), StatusCode::OK);
    assert!(inbound_rx.try_recv().is_err());
}

// ── Registration YAML tests ─────────────────────────────────────────

#[cfg(unix)]
#[test]
fn test_matrix_registration_file_mode_is_owner_only() {
    let ch = make_channel();
    let tmp = tempfile::tempdir().unwrap();
    let path = ch.generate_registration(tmp.path()).unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "registration YAML must not be group/world accessible: {:o}",
        mode & 0o777
    );
}

// ── BotRouter tests ─────────────────────────────────────────────────

#[tokio::test]
async fn test_matrix_register_bot_fails_when_route_persist_fails() {
    let (homeserver, _requests, homeserver_handle) = spawn_mock_homeserver().await;
    let tmp = tempfile::tempdir().unwrap();
    let missing_data_dir = tmp.path().join("missing-dir");
    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9880,
        Arc::new(AtomicBool::new(false)),
    )
    .with_bot_router(&missing_data_dir);

    let result = ch
        .register_bot("@octos_weather:localhost", "profile-weather")
        .await;
    assert!(
        result.is_err(),
        "register_bot should fail when route persistence fails"
    );
    assert_eq!(
        ch.bot_router().route("@octos_weather:localhost").await,
        None,
        "failed registration should not leave an in-memory route"
    );
    let users = ch.registered_users.read().await;
    assert!(
        !users.contains("@octos_weather:localhost"),
        "failed registration should not leave sender authorization"
    );

    homeserver_handle.abort();
}

// ── Track A: sender_user_id tests ──────────────────────────────────────

#[tokio::test]
async fn test_matrix_send_rejects_unregistered_sender() {
    let (homeserver, _requests, handle) = spawn_mock_homeserver().await;
    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        unused_local_port(),
        Arc::new(AtomicBool::new(false)),
    );
    // Do NOT register @octos_unknown:localhost

    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "Hello from unknown bot".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({"sender_user_id": "@octos_unknown:localhost"}),
    };

    let result = ch.send_with_id(&msg).await;
    assert!(result.is_err(), "should reject unregistered sender_user_id");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not registered"),
        "error should mention 'not registered', got: {err_msg}"
    );

    handle.abort();
}

// ── DM routing (room-bot mapping) tests ─────────────────────────

#[tokio::test]
async fn test_private_bot_message_blocked_for_non_owner() {
    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;

    let router = BotRouter::new(None);
    router
        .register_entry(
            "@octos_private:localhost",
            "main--private",
            "@owner:localhost",
            BotVisibility::Private,
        )
        .await
        .unwrap();
    router
        .add_room_bot("!private:localhost", "main--private")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = transaction_app(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@mallory:localhost",
            "room_id": "!private:localhost",
            "event_id": "$private1",
            "content": {
                "msgtype": "m.text",
                "body": "hello private bot"
            }
        }]
    });

    let resp = put_transaction(
        app,
        "txn-private-msg",
        Body::from(serde_json::to_string(&body).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        inbound_rx.try_recv().is_err(),
        "non-owner message should not be forwarded to the agent"
    );

    // Unaddressed room chatter routed to a private bot only via the room
    // mapping is dropped SILENTLY: replying would spam the room and
    // reveal the private bot's existence.
    let requests = requests.lock().await;
    assert!(
        !requests.iter().any(|req| req.path.contains("/send/")),
        "unaddressed non-owner message must not trigger a rejection reply"
    );

    homeserver_handle.abort();
}

// ── Slash command parsing tests ──────────────────────────────────

#[tokio::test]
async fn should_enforce_target_cap_when_allbots_invoked() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let targets = (0..=MAX_ALLBOTS_TARGETS)
        .map(|i| format!("@octos_child_{i}:localhost"))
        .collect::<Vec<_>>();

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/allbots summarize this issue",
        Some("@octos_bot:localhost"),
        &json!({
            "org.octos.broadcast_targets": targets,
        }),
        None,
    )
    .await;

    assert_eq!(
        result,
        Some(format!(
            "/allbots can target at most {MAX_ALLBOTS_TARGETS} bound child bots at once."
        )),
    );
}

#[tokio::test]
async fn should_fan_out_allbots_to_bound_child_bots() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let router = BotRouter::new(None);
    router
        .register("@octos_bot:localhost", "profile-parent")
        .await
        .unwrap();
    router
        .register("@octos_alex:localhost", "profile-alex")
        .await
        .unwrap();
    router
        .register("@octos_bob:localhost", "profile-bob")
        .await
        .unwrap();
    // A globally registered child bound only to a different room must be
    // filtered from the same forged broadcast request.
    router
        .register("@octos_elsewhere:localhost", "profile-elsewhere")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-alex")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-bob")
        .await
        .unwrap();
    router
        .add_room_bot("!other:localhost", "profile-elsewhere")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = transaction_app(state);

    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$allbots-fanout-1",
            "content": {
                "msgtype": "m.text",
                "body": "/allbots summarize this issue",
                "org.octos.target_user_id": "@octos_bot:localhost",
                "org.octos.broadcast_targets": [
                    "@octos_alex:localhost",
                    "@octos_bob:localhost",
                    "@octos_elsewhere:localhost"
                ]
            }
        }]
    });

    let resp = put_transaction(
        app,
        "txn-allbots-fanout-1",
        Body::from(serde_json::to_string(&body).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let mut seen = Vec::new();
    while let Ok(msg) = inbound_rx.try_recv() {
        seen.push((
            msg.content,
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
        ));
    }

    seen.sort();

    assert_eq!(
        seen,
        vec![
            (
                "summarize this issue".to_string(),
                Some("profile-alex".to_string()),
            ),
            (
                "summarize this issue".to_string(),
                Some("profile-bob".to_string()),
            ),
        ],
        "/allbots must fan out only to children bound to this room, never the cross-room target",
    );
}

/// Mock BotManager for testing slash command dispatch.
struct MockBotManager;

#[async_trait]
impl BotManager for MockBotManager {
    async fn create_bot(
        &self,
        username: &str,
        name: &str,
        _system_prompt: Option<&str>,
        _sender: &str,
        visibility: BotVisibility,
    ) -> Result<String> {
        Ok(format!("mock create: {username} ({name}) {visibility:?}"))
    }
    async fn delete_bot(&self, matrix_user_id: &str, _sender: &str) -> Result<String> {
        Ok(format!("mock delete: {matrix_user_id}"))
    }
    async fn list_bots(&self, _sender: &str) -> Result<String> {
        Ok("mock list: no bots".to_string())
    }

    async fn schedule_bot_task(
        &self,
        request: &str,
        _sender: &str,
        room_id: &str,
    ) -> Result<String> {
        Ok(format!("mock schedule: {request} in {room_id}"))
    }
    async fn list_schedules(&self, _sender: &str, room_id: &str) -> Result<String> {
        Ok(format!("mock schedules for {room_id}"))
    }
    async fn unschedule_bot_task(
        &self,
        job_id: &str,
        _sender: &str,
        room_id: &str,
    ) -> Result<String> {
        Ok(format!("mock unschedule: {job_id} in {room_id}"))
    }
}
