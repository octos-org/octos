use super::*;
use std::time::Duration;

use axum::extract::State;
use axum::http::{Method, Uri};
use axum::routing::any;
use tokio::sync::Mutex;

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
        media_dir: std::env::temp_dir().join("octos-matrix-test-media"),
        // Existing transaction tests exercise routing/enqueue without the
        // mention gate; enable it explicitly in the gate's own tests.
        mention_only: false,
        dm_member_cache: Arc::new(RwLock::new(HashMap::new())),
        dm_member_cache_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    }
}

// ── mention-only gate ────────────────────────────────────────────────

fn members(humans: usize, managed_bots: usize) -> RoomMemberCounts {
    RoomMemberCounts {
        humans,
        managed_bots,
    }
}

#[test]
fn gate_disabled_always_dispatches() {
    // Gate off → legacy behaviour, reply regardless of room.
    assert!(should_dispatch_message(false, false, members(5, 3)));
    assert!(should_dispatch_message(false, false, members(1, 1)));
}

#[test]
fn gate_dispatches_when_addressed() {
    // Explicit mention/target bypasses the member-count check entirely.
    assert!(should_dispatch_message(true, true, members(100, 100)));
}

#[test]
fn gate_replies_in_dm_without_mention() {
    // A true 1:1 DM (one human, one child bot) replies without a mention.
    assert!(should_dispatch_message(true, false, members(1, 1)));
    assert!(should_dispatch_message(true, false, members(0, 1)));
    assert!(should_dispatch_message(true, false, members(1, 0)));
}

#[test]
fn gate_requires_mention_in_group() {
    // Multiple humans and no mention → do not reply.
    assert!(!should_dispatch_message(true, false, members(2, 1)));
    assert!(!should_dispatch_message(true, false, members(9, 1)));
}

#[test]
fn gate_requires_mention_when_room_has_multiple_bots() {
    // One human sharing a room with several bots is NOT a DM: without
    // this, every bot in the room would answer each unaddressed message.
    assert!(!should_dispatch_message(true, false, members(1, 2)));
    assert!(!should_dispatch_message(true, false, members(0, 5)));
}

#[test]
fn gate_fails_closed_when_membership_unknown() {
    assert!(!should_dispatch_message(
        true,
        false,
        RoomMemberCounts::UNKNOWN
    ));
}

fn membership(humans: usize, child_bots: usize, botfather_joined: bool) -> MembershipCounts {
    MembershipCounts {
        humans,
        child_bots,
        botfather_joined,
    }
}

fn mapped(child_bots: usize, botfather_mapped: bool) -> RoomBotComposition {
    RoomBotComposition {
        child_bots,
        botfather_mapped,
    }
}

#[test]
fn count_room_members_splits_humans_child_bots_and_botfather() {
    let joined = json!({
        "joined": {
            "@octos_bot:localhost": { "display_name": "Octos" },
            "@octos_alexbot:localhost": { "display_name": "AlexBot" },
            "@alice:localhost": { "display_name": "Alice" },
            "@bob:localhost": { "display_name": "Bob" }
        }
    });
    assert_eq!(
        count_room_members(&joined, "@octos_bot:localhost", ":localhost", "octos_"),
        Some(membership(2, 1, true))
    );
}

#[test]
fn count_room_members_handles_missing_field() {
    // No `joined` object → membership unknown → merge fails closed.
    assert_eq!(
        count_room_members(&json!({}), "@octos_bot:localhost", ":localhost", "octos_"),
        None
    );
    assert_eq!(
        merge_room_bot_sources(None, Some(mapped(1, false))),
        RoomMemberCounts::UNKNOWN
    );
}

#[test]
fn merge_unions_membership_and_room_map() {
    // Palpo hides child virtual users from joined_members: the room map
    // is the authority on child bots, membership on humans/BotFather.
    let hidden_children =
        merge_room_bot_sources(Some(membership(1, 0, false)), Some(mapped(2, false)));
    assert_eq!(hidden_children, members(1, 2));
    assert!(!should_dispatch_message(true, false, hidden_children));

    // A single mapped child bot with one human stays a DM.
    let dm = merge_room_bot_sources(Some(membership(1, 0, false)), Some(mapped(1, false)));
    assert_eq!(dm, members(1, 1));
    assert!(dm.is_direct_chat());

    // Membership-visible children and mapped children are the same
    // population seen through different windows: max, not sum.
    let both_visible =
        merge_room_bot_sources(Some(membership(1, 1, false)), Some(mapped(1, false)));
    assert_eq!(both_visible, members(1, 1));
}

#[test]
fn merge_counts_botfather_and_hidden_child_as_two_responders() {
    // The P1 review case: one human + BotFather (visible in membership)
    // + one child bot (hidden by the homeserver, known to the room map).
    // Each source alone sees "1 bot"; a max of totals would grant a DM
    // exemption. The union must see two potential responders.
    let counts = merge_room_bot_sources(Some(membership(1, 0, true)), Some(mapped(1, false)));
    assert_eq!(counts, members(1, 2));
    assert!(!counts.is_direct_chat());
    assert!(!should_dispatch_message(true, false, counts));
}

#[test]
fn merge_botfather_only_dm_is_direct_chat() {
    // 1:1 admin chat with BotFather alone stays a DM, whether membership
    // or the room map is the source that sees it.
    let via_membership =
        merge_room_bot_sources(Some(membership(1, 0, true)), Some(mapped(0, false)));
    assert_eq!(via_membership, members(1, 1));
    assert!(via_membership.is_direct_chat());

    let via_room_map = merge_room_bot_sources(Some(membership(1, 0, false)), Some(mapped(0, true)));
    assert_eq!(via_room_map, members(1, 1));
    assert!(via_room_map.is_direct_chat());

    // Both sources seeing BotFather is still one BotFather.
    let both = merge_room_bot_sources(Some(membership(1, 0, true)), Some(mapped(0, true)));
    assert_eq!(both, members(1, 1));
}

#[test]
fn merge_fails_closed_when_room_map_unavailable() {
    // A poisoned room map (existing file that failed to load) yields
    // `None` from `room_bot_composition`; the gate must not fall back to
    // membership alone, which may be blind to hidden child bots.
    assert_eq!(
        merge_room_bot_sources(Some(membership(1, 0, false)), None),
        RoomMemberCounts::UNKNOWN
    );
}

#[tokio::test]
async fn room_bot_composition_distinguishes_botfather_from_children() {
    let router = BotRouter::new(None);
    router
        .register("@octos_bot:localhost", "botfather")
        .await
        .unwrap();
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "botfather")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-weather")
        .await
        .unwrap();

    assert_eq!(
        router
            .room_bot_composition("!room:localhost", "@octos_bot:localhost")
            .await,
        Some(mapped(1, true))
    );
    // A room absent from the map has no mapped bots (legitimate, e.g. a
    // room the appservice was never bound to) — not a failure.
    assert_eq!(
        router
            .room_bot_composition("!other:localhost", "@octos_bot:localhost")
            .await,
        Some(mapped(0, false))
    );
}

#[test]
fn mentions_user_detects_structured_and_text_mentions() {
    let bot = "@octosbot:localhost";

    // Structured m.mentions.user_ids
    let structured = json!({ "m.mentions": { "user_ids": ["@octosbot:localhost"] } });
    assert!(mentions_user(&structured, "hello", bot));

    // Plain-text MXID mention in the body
    let text = json!({});
    assert!(mentions_user(
        &text,
        "hey @octosbot:localhost please help",
        bot
    ));

    // MXID mention embedded in formatted_body markup.
    let formatted = json!({ "formatted_body": "<b>@octosbot:localhost</b> hi" });
    assert!(mentions_user(&formatted, "hi", bot));

    // Standard matrix.to pill: the MXID is preceded by `/` in the href,
    // which must not defeat the left-boundary check.
    let pill = json!({
        "formatted_body":
            "<a href=\"https://matrix.to/#/@octosbot:localhost\">octosbot</a> hi"
    });
    assert!(mentions_user(&pill, "octosbot hi", bot));

    // No mention at all
    let none = json!({});
    assert!(!mentions_user(&none, "just chatting with everyone", bot));
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: Method,
    path: String,
    query: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct MockHomeserverState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    status: StatusCode,
    response_body: Value,
}

async fn capture_homeserver_request(
    State(state): State<MockHomeserverState>,
    method: Method,
    uri: Uri,
    body: String,
) -> impl IntoResponse {
    let body = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }))
    };
    state.requests.lock().await.push(CapturedRequest {
        method,
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        body,
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

async fn spawn_mock_homeserver_with_joined_members(
    joined_members: Value,
) -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_homeserver_with_response(StatusCode::OK, joined_members).await
}

async fn spawn_mock_homeserver_with_dynamic_event_ids() -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    async fn dynamic_handler(
        State(requests): State<Arc<Mutex<Vec<CapturedRequest>>>>,
        method: Method,
        uri: Uri,
        body: String,
    ) -> impl IntoResponse {
        let body = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }))
        };
        requests.lock().await.push(CapturedRequest {
            method,
            path: uri.path().to_string(),
            query: uri.query().map(str::to_string),
            body,
        });
        let event_id = uri
            .path()
            .rsplit('/')
            .next()
            .map(|txn_id| format!("${txn_id}"))
            .unwrap_or_else(|| "$missing_txn".to_string());
        (
            StatusCode::OK,
            serde_json::to_string(&json!({ "event_id": event_id })).unwrap(),
        )
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
            any(dynamic_handler),
        )
        .with_state(requests.clone());
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

async fn wait_for_request_count(requests: &Arc<Mutex<Vec<CapturedRequest>>>, min_count: usize) {
    for _ in 0..20 {
        if requests.lock().await.len() >= min_count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn test_matrix_channel_name() {
    let ch = make_channel();
    assert_eq!(ch.name(), "matrix");
}

#[test]
fn test_matrix_supports_edit() {
    let ch = make_channel();
    assert!(ch.supports_edit());
}

#[test]
fn test_matrix_max_message_length() {
    let ch = make_channel();
    assert_eq!(ch.max_message_length(), 65535);
}

#[test]
fn test_matrix_bot_user_id() {
    let ch = make_channel();
    assert_eq!(ch.bot_user_id(), "@octos_bot:localhost");
}

#[test]
fn test_make_api_url() {
    let ch = make_channel();
    assert_eq!(
        ch.make_api_url("/_matrix/client/v3/account/whoami"),
        "http://localhost:6167/_matrix/client/v3/account/whoami"
    );
}

#[test]
fn test_make_api_url_strips_trailing_slash() {
    let ch = MatrixChannel::new(
        "http://localhost:6167/",
        "as",
        "hs",
        "localhost",
        "bot",
        "octos_",
        9880,
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        ch.make_api_url("/_matrix/client/v3/whoami"),
        "http://localhost:6167/_matrix/client/v3/whoami"
    );
}

#[test]
fn test_default_appservice_bind_addr_uses_all_interfaces() {
    assert_eq!(default_appservice_bind_addr(9880), "0.0.0.0:9880");
}

#[test]
fn test_is_managed_user_bot() {
    assert!(is_managed_user(
        "@octos_bot:localhost",
        "@octos_bot:localhost",
        ":localhost",
        "octos_",
    ));
}

#[test]
fn test_is_managed_user_virtual_user() {
    assert!(is_managed_user(
        "@octos_agent1:localhost",
        "@octos_bot:localhost",
        ":localhost",
        "octos_",
    ));
}

#[test]
fn test_is_managed_user_regular_user() {
    assert!(!is_managed_user(
        "@alice:localhost",
        "@octos_bot:localhost",
        ":localhost",
        "octos_",
    ));
}

#[test]
fn test_is_managed_user_other_server() {
    assert!(!is_managed_user(
        "@octos_bot:other.server",
        "@octos_bot:localhost",
        ":localhost",
        "octos_",
    ));
}

// ── Token validation tests ───────────────────────────────────────────

#[test]
fn test_validate_hs_token_query_valid() {
    let query = AccessTokenQuery {
        access_token: Some("secret".to_string()),
    };
    let headers = HeaderMap::new();
    assert!(validate_hs_token(&query, &headers, "secret").is_ok());
}

#[test]
fn test_validate_hs_token_query_invalid() {
    let query = AccessTokenQuery {
        access_token: Some("wrong".to_string()),
    };
    let headers = HeaderMap::new();
    let result = validate_hs_token(&query, &headers, "secret");
    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

#[test]
fn test_validate_hs_token_bearer_valid() {
    let query = AccessTokenQuery { access_token: None };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer secret".parse().unwrap());
    assert!(validate_hs_token(&query, &headers, "secret").is_ok());
}

#[test]
fn test_validate_hs_token_bearer_invalid() {
    let query = AccessTokenQuery { access_token: None };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer wrong".parse().unwrap());
    let result = validate_hs_token(&query, &headers, "secret");
    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

#[test]
fn test_validate_hs_token_missing() {
    let query = AccessTokenQuery { access_token: None };
    let headers = HeaderMap::new();
    let result = validate_hs_token(&query, &headers, "secret");
    assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
}

#[test]
fn test_validate_hs_token_rejects_mismatched_query_and_header() {
    let query = AccessTokenQuery {
        access_token: Some("secret".to_string()),
    };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer wrong".parse().unwrap());
    let result = validate_hs_token(&query, &headers, "secret");
    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

// ── txn_id dedup test ────────────────────────────────────────────────

#[test]
fn test_txn_id_dedup() {
    let ch = make_channel();

    // First time: not a duplicate
    assert!(!ch.dedup.is_duplicate("txn_1"));

    // Second time: duplicate
    assert!(ch.dedup.is_duplicate("txn_1"));
}

// ── Registered users test ────────────────────────────────────────────

#[tokio::test]
async fn test_registered_users() {
    let ch = make_channel();
    {
        let mut users = ch.registered_users.write().await;
        users.insert("@octos_bot:localhost".to_string());
    }
    let users = ch.registered_users.read().await;
    assert!(users.contains("@octos_bot:localhost"));
    assert!(!users.contains("@other:localhost"));
}

// ── Axum handler integration tests ───────────────────────────────────

#[tokio::test]
async fn test_handle_transaction_missing_token() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let mut state = make_test_state(inbound_tx);
    state.hs_token = "correct_token".to_string();

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({"events": []});

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn1")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_handle_transaction_bad_json_does_not_poison_txn_id() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let bad_req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_retry?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from("{not-json"))
        .unwrap();

    let bad_resp = app.clone().oneshot(bad_req).await.unwrap();
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
    let good_req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_retry?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&good_body).unwrap()))
        .unwrap();

    let good_resp = app.oneshot(good_req).await.unwrap();
    assert_eq!(good_resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(msg.content, "retry should still deliver");
}

#[tokio::test]
async fn test_handle_transaction_ignores_bot_messages() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@octos_bot:localhost",
            "room_id": "!room:localhost",
            "event_id": "$ev_bot",
            "content": {
                "msgtype": "m.text",
                "body": "bot's own message"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_bot?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Bot's own message should be ignored
    assert!(inbound_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_handle_ping() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);
    let app = Router::new()
        .route("/_matrix/app/v1/ping", axum::routing::post(handle_ping))
        .with_state(state);

    let req = Request::builder()
        .method("POST")
        .uri("/_matrix/app/v1/ping?access_token=test_token")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_handle_ping_requires_token() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);
    let app = Router::new()
        .route("/_matrix/app/v1/ping", axum::routing::post(handle_ping))
        .with_state(state);

    let req = Request::builder()
        .method("POST")
        .uri("/_matrix/app/v1/ping")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_handle_user_query_bot() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);

    let state = make_test_state(inbound_tx);
    state
        .registered_users
        .write()
        .await
        .insert("@octos_agent1:localhost".to_string());

    let app = Router::new()
        .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
        .with_state(state);

    // Query for bot user — should return 200
    let req = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@octos_bot:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Query for virtual user — should return 200
    let req2 = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@octos_agent1:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();

    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // Query for unknown user — should return 404
    let req3 = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@alice:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();

    let resp3 = app.oneshot(req3).await.unwrap();
    assert_eq!(resp3.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_handle_user_query_unknown_managed_user_returns_404() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.as_token = "test_as_token".to_string();

    let app = Router::new()
        .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
        .with_state(state);

    let req = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@octos_unknown:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let requests = requests.lock().await;
    assert!(
        requests.is_empty(),
        "unknown managed user query should not auto-register with homeserver"
    );

    homeserver_handle.abort();
}

#[tokio::test]
async fn test_handle_room_query_requires_token() {
    let appservice_port = unused_local_port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let channel = Arc::new(MatrixChannel::new(
        "http://localhost:6167",
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        appservice_port,
        shutdown,
    ));

    let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let channel_task = {
        let channel = channel.clone();
        tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() })
    };

    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://127.0.0.1:{appservice_port}/_matrix/app/v1/rooms/%23alias%3Alocalhost"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    channel.stop().await.unwrap();
    channel_task.await.unwrap();
}

#[tokio::test]
async fn test_handle_transaction_invite_joins_room() {
    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let appservice_port = unused_local_port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let channel = Arc::new(MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        appservice_port,
        shutdown,
    ));

    let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let channel_task = {
        let channel = channel.clone();
        tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() })
    };

    wait_for_request_count(&requests, 1).await;

    let body = json!({
        "events": [{
            "type": "m.room.member",
            "room_id": "!room123:localhost",
            "state_key": "@octos_agent1:localhost",
            "content": {
                "membership": "invite"
            }
        }]
    });

    let client = reqwest::Client::new();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let http_resp = client
            .put(format!(
                "http://127.0.0.1:{appservice_port}/_matrix/app/v1/transactions/txn-invite?access_token=hs_token_test"
            ))
            .header("content-type", "application/json")
            .body(serde_json::to_string(&body).unwrap())
            .send()
            .await
            .unwrap();
    assert_eq!(http_resp.status(), StatusCode::OK);

    wait_for_request_count(&requests, 2).await;
    let requests = requests.lock().await;
    assert!(requests.iter().any(|req| {
        req.method == Method::POST
            && req.path == "/_matrix/client/v3/rooms/%21room123%3Alocalhost/join"
            && req
                .query
                .as_deref()
                .is_some_and(|q| q.contains("user_id=%40octos_agent1%3Alocalhost"))
    }));

    channel.stop().await.unwrap();
    channel_task.await.unwrap();
    homeserver_handle.abort();
}

#[tokio::test]
async fn test_private_bot_invite_rejected_for_non_owner() {
    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
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
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state.clone());

    let body = json!({
        "events": [{
            "type": "m.room.member",
            "sender": "@mallory:localhost",
            "room_id": "!room123:localhost",
            "state_key": "@octos_private:localhost",
            "content": {
                "membership": "invite"
            }
        }]
    });

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-private-invite?access_token=test_token")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&body).unwrap(),
        ))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_request_count(&requests, 3).await;
    let requests = requests.lock().await;
    assert!(requests.iter().any(|req| req.path.ends_with("/join")));
    assert!(requests.iter().any(|req| req.path.contains("/send/")));
    assert!(requests.iter().any(|req| req.path.ends_with("/leave")));
    assert_eq!(
        state.bot_router.route_by_room("!room123:localhost").await,
        None,
        "private bot should not persist room mapping for non-owner invite"
    );

    homeserver_handle.abort();
}

#[tokio::test]
async fn test_health_check_includes_user_id() {
    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9880,
        Arc::new(AtomicBool::new(false)),
    );

    let health = ch.health_check().await.unwrap();
    assert_eq!(health, ChannelHealth::Healthy);

    wait_for_request_count(&requests, 1).await;
    let requests = requests.lock().await;
    assert!(requests.iter().any(|req| {
        req.path == "/_matrix/client/v3/account/whoami"
            && req
                .query
                .as_deref()
                .is_some_and(|q| q.contains("user_id=%40octos_bot%3Alocalhost"))
    }));

    homeserver_handle.abort();
}

#[tokio::test]
async fn test_stop_sets_shutdown() {
    let ch = make_channel();
    assert!(!ch.shutdown.load(Ordering::Acquire));
    ch.stop().await.unwrap();
    assert!(ch.shutdown.load(Ordering::Acquire));
}

#[test]
fn test_matrix_supports_edit_true() {
    let ch = make_channel();
    assert!(ch.supports_edit());
}

#[tokio::test]
async fn test_matrix_appservice_receives_message() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room123:localhost",
            "event_id": "$event1",
            "content": {
                "msgtype": "m.text",
                "body": "hello from matrix"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(msg.channel, "matrix");
    assert_eq!(msg.sender_id, "@alice:elsewhere.org");
    assert_eq!(msg.chat_id, "!room123:localhost");
    assert_eq!(msg.content, "hello from matrix");
}

#[tokio::test]
async fn test_matrix_rejects_invalid_hs_token() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.hs_token = "correct_token".to_string();

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn1?access_token=wrong_token")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"events":[]}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_matrix_dedup_txn_id() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

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

#[tokio::test]
async fn test_matrix_user_query() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);
    state
        .registered_users
        .write()
        .await
        .insert("@octos_agent1:localhost".to_string());

    let app = Router::new()
        .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
        .with_state(state);

    let bot_req = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@octos_bot:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(bot_req).await.unwrap().status(),
        StatusCode::OK
    );

    let unknown_req = Request::builder()
        .method("GET")
        .uri("/_matrix/app/v1/users/@alice:localhost?access_token=test_token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(unknown_req).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_matrix_send_message() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "hello from matrix".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };

    ch.send(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let send_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    assert_eq!(send_req.method, Method::PUT);
    assert_eq!(send_req.body["body"], "hello from matrix");

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_with_id() {
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
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "hello from matrix".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };

    let event_id = ch.send_with_id(&msg).await.unwrap();
    assert_eq!(event_id.as_deref(), Some("$test_event"));

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_no_live() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "regular".into(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };
    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    assert!(req.body.get(LIVE_MARKER).is_none());
    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_projects_app_metadata_into_event_content() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "mission update".into(),
        reply_to: None,
        media: vec![],
        metadata: json!({
            CONTENT_APP: {
                "type": "mission_room",
                "version": 1,
                "scope": "room",
                "app_id": "mission:alpha",
                "initial_state": { "status": "green" }
            },
            CONTENT_ACTIONS: [{
                "id": "ack",
                "label": "Acknowledge"
            }],
            CONTENT_ACTION_RESPONSE: {
                "action_id": "ack",
                "state": { "acknowledged": true }
            }
        }),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    assert_eq!(req.body[CONTENT_APP]["type"], json!("mission_room"));
    assert_eq!(req.body[CONTENT_ACTIONS][0]["id"], json!("ack"));
    assert_eq!(
        req.body[CONTENT_ACTION_RESPONSE]["state"]["acknowledged"],
        json!(true)
    );

    handle.abort();
}

#[tokio::test]
async fn should_upload_and_send_m_image_when_outbound_has_media() {
    let (homeserver, requests, handle) = spawn_mock_homeserver_with_response(
        StatusCode::OK,
        json!({
            "event_id": "$media_event",
            "content_uri": "mxc://localhost/abc123"
        }),
    )
    .await;
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

    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("photo.png");
    std::fs::write(&file, b"fake png bytes").unwrap();

    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "look at this".into(),
        reply_to: None,
        media: vec![file.to_string_lossy().into_owned()],
        metadata: json!({}),
    };

    let event_id = ch.send_with_id(&msg).await.unwrap();
    assert_eq!(event_id.as_deref(), Some("$media_event"));

    wait_for_request_count(&requests, 2).await;
    let reqs = requests.lock().await;

    let upload = reqs
        .iter()
        .find(|r| r.path.contains("/_matrix/media/v3/upload"))
        .expect("should have an upload request");
    assert_eq!(upload.method, Method::POST);
    let query = upload.query.as_deref().unwrap_or("");
    assert!(query.contains("filename=photo.png"), "query: {query}");

    let send = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a media send request");
    assert_eq!(send.body["msgtype"], json!("m.image"));
    assert_eq!(send.body["url"], json!("mxc://localhost/abc123"));
    assert_eq!(send.body["body"], json!("look at this"));
    assert_eq!(send.body["filename"], json!("photo.png"));
    assert_eq!(send.body["info"]["mimetype"], json!("image/png"));

    handle.abort();
}

#[tokio::test]
async fn should_caption_first_file_only_when_outbound_has_multiple_media() {
    let (homeserver, requests, handle) = spawn_mock_homeserver_with_response(
        StatusCode::OK,
        json!({
            "event_id": "$media_event",
            "content_uri": "mxc://localhost/abc123"
        }),
    )
    .await;
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

    let dir = tempfile::TempDir::new().unwrap();
    let img = dir.path().join("a.png");
    let doc = dir.path().join("b.pdf");
    std::fs::write(&img, b"img").unwrap();
    std::fs::write(&doc, b"doc").unwrap();

    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "the caption".into(),
        reply_to: None,
        media: vec![
            img.to_string_lossy().into_owned(),
            doc.to_string_lossy().into_owned(),
        ],
        metadata: json!({}),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 4).await;
    let reqs = requests.lock().await;
    let sends: Vec<_> = reqs.iter().filter(|r| r.path.contains("/send/")).collect();
    assert_eq!(sends.len(), 2, "expected one send per media file");
    assert_eq!(sends[0].body["msgtype"], json!("m.image"));
    assert_eq!(sends[0].body["body"], json!("the caption"));
    assert_eq!(sends[1].body["msgtype"], json!("m.file"));
    // Second file gets its filename as body, not the caption.
    assert_eq!(sends[1].body["body"], json!("b.pdf"));
    assert_eq!(sends[1].body["info"]["mimetype"], json!("application/pdf"));

    handle.abort();
}

#[tokio::test]
async fn should_download_media_and_attach_path_when_inbound_image_event() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // Mock homeserver serves the media download endpoint with canned bytes.
    let (homeserver, hs_requests, hs_handle) =
        spawn_mock_homeserver_with_response(StatusCode::OK, json!({"bytes": "fake"})).await;

    let media_dir = tempfile::TempDir::new().unwrap();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.media_dir = media_dir.path().to_path_buf();

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$ev_media",
            "content": {
                "msgtype": "m.image",
                "body": "cat.png",
                "url": "mxc://localhost/catmedia",
                "filename": "cat.png"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_media?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let inbound = inbound_rx.recv().await.expect("inbound media message");
    assert_eq!(inbound.content, "cat.png");
    assert_eq!(inbound.media.len(), 1, "media path should be attached");
    let local = std::path::Path::new(&inbound.media[0]);
    assert!(local.exists(), "downloaded file should exist: {local:?}");
    assert!(
        inbound.media[0].contains("cat.png"),
        "local path should keep the original filename: {}",
        inbound.media[0]
    );

    let reqs = hs_requests.lock().await;
    assert!(
        reqs.iter().any(|r| r
            .path
            .contains("/_matrix/media/v3/download/localhost/catmedia")),
        "should have hit the media download endpoint"
    );

    hs_handle.abort();
}

#[tokio::test]
async fn should_deliver_text_only_when_media_download_fails() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let media_dir = tempfile::TempDir::new().unwrap();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    // Unreachable homeserver — download must fail, message must still flow.
    state.homeserver = "http://127.0.0.1:1".to_string();
    state.media_dir = media_dir.path().to_path_buf();

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$ev_media_fail",
            "content": {
                "msgtype": "m.image",
                "body": "cat.png",
                "url": "mxc://localhost/gonemedia"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_media_fail?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let inbound = inbound_rx.recv().await.expect("inbound message");
    assert_eq!(inbound.content, "cat.png");
    assert!(
        inbound.media.is_empty(),
        "failed download should degrade to text-only"
    );
}

#[tokio::test]
async fn test_matrix_edit_message() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    ch.edit_message("!room:localhost", "$event1", "**bold** text")
        .await
        .unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let edit_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have an edit request");
    assert_eq!(edit_req.body["format"], HTML_FORMAT);
    assert_eq!(edit_req.body["m.relates_to"]["rel_type"], REL_TYPE_REPLACE);
    assert_eq!(edit_req.body["m.relates_to"]["event_id"], "$event1");
    assert!(edit_req.body["formatted_body"].is_string());

    handle.abort();
}

#[tokio::test]
async fn test_matrix_live_lifecycle() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    // Initial streaming message
    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "first".into(),
        reply_to: None,
        media: vec![],
        metadata: json!({"streaming": true}),
    };
    let eid = ch.send_with_id(&msg).await.unwrap().unwrap();
    // Intermediate edit
    ch.edit_message("!room:localhost", &eid, "partial")
        .await
        .unwrap();
    // Final
    ch.finish_stream("!room:localhost", &eid, "done")
        .await
        .unwrap();

    wait_for_request_count(&requests, 3).await;
    let reqs = requests.lock().await;
    let sends: Vec<_> = reqs.iter().filter(|r| r.path.contains("/send/")).collect();
    assert_eq!(sends.len(), 3);
    // Initial and edit carry live marker
    assert_eq!(sends[0].body[LIVE_MARKER], json!({}));
    assert_eq!(sends[1].body[LIVE_MARKER], json!({}));
    assert_eq!(sends[1].body["m.new_content"][LIVE_MARKER], json!({}));
    // Finish omits it
    assert!(sends[2].body.get(LIVE_MARKER).is_none());
    assert!(sends[2].body["m.new_content"].get(LIVE_MARKER).is_none());
    assert_eq!(sends[2].body["m.new_content"]["body"], "done");
    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_typing() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    ch.send_typing("!room:localhost").await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let typing_req = reqs
        .iter()
        .find(|r| r.path.contains("/typing/"))
        .expect("should have a typing request");
    assert_eq!(typing_req.method, Method::PUT);
    assert_eq!(typing_req.body["typing"], true);
    assert_eq!(typing_req.body["timeout"], 30000);

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_typing_as() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    ch.send_typing_as("!room:localhost", Some("@octos_weather:localhost"))
        .await
        .unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let typing_req = reqs
        .iter()
        .find(|r| r.path.contains("/typing/"))
        .expect("should have a typing request");
    assert_eq!(typing_req.method, Method::PUT);
    assert!(
        typing_req.path.contains("%40octos_weather%3Alocalhost"),
        "typing path should use sender identity, got: {}",
        typing_req.path
    );
    let query = typing_req.query.as_deref().unwrap_or("");
    assert!(
        query.contains("user_id=%40octos_weather%3Alocalhost"),
        "typing query should use sender identity, got: {query}"
    );

    handle.abort();
}

#[tokio::test]
async fn test_matrix_stop_typing_as() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    ch.stop_typing_as("!room:localhost", Some("@octos_weather:localhost"))
        .await
        .unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let typing_req = reqs
        .iter()
        .find(|r| r.path.contains("/typing/"))
        .expect("should have a typing request");
    assert_eq!(typing_req.method, Method::PUT);
    assert_eq!(typing_req.body["typing"], false);
    assert!(
        typing_req.path.contains("%40octos_weather%3Alocalhost"),
        "typing path should use sender identity, got: {}",
        typing_req.path
    );
    let query = typing_req.query.as_deref().unwrap_or("");
    assert!(
        query.contains("user_id=%40octos_weather%3Alocalhost"),
        "typing query should use sender identity, got: {query}"
    );

    handle.abort();
}

#[tokio::test]
async fn test_matrix_health_check() {
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

    let health = ch.health_check().await.unwrap();
    assert_eq!(health, ChannelHealth::Healthy);

    handle.abort();
}

#[tokio::test]
async fn test_matrix_health_check_down() {
    let (homeserver, _requests, handle) = spawn_mock_homeserver_with_response(
        StatusCode::BAD_GATEWAY,
        json!({"error": "homeserver unavailable"}),
    )
    .await;
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

    let health = ch.health_check().await.unwrap();
    assert!(matches!(health, ChannelHealth::Down(_)));

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_html_format() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "**bold** text".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let send_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    assert_eq!(send_req.body["format"], HTML_FORMAT);
    assert!(send_req.body["formatted_body"].is_string());
    assert_eq!(send_req.body["body"], "**bold** text");

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_typing_failure_ignored() {
    let (homeserver, _requests, handle) = spawn_mock_homeserver_with_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "typing failed"}),
    )
    .await;
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

    ch.send_typing("!room:localhost").await.unwrap();

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_then_edit_flow() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "initial text".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };

    let event_id = ch.send_with_id(&msg).await.unwrap().unwrap();
    ch.edit_message("!room:localhost", &event_id, "updated text")
        .await
        .unwrap();

    wait_for_request_count(&requests, 2).await;
    let reqs = requests.lock().await;
    let send_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/") && r.body.get("m.relates_to").is_none())
        .expect("should have an initial send request");
    let edit_req = reqs
        .iter()
        .find(|r| r.body.get("m.relates_to").is_some())
        .expect("should have an edit request");
    assert_eq!(send_req.body["body"], "initial text");
    assert_eq!(edit_req.body["m.relates_to"]["event_id"], "$test_event");
    assert_eq!(edit_req.body["m.new_content"]["body"], "updated text");

    handle.abort();
}

#[tokio::test]
async fn test_matrix_event_sender_cache_is_bounded() {
    let (homeserver, _requests, handle) = spawn_mock_homeserver_with_dynamic_event_ids().await;
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
    let sender = "@octos_weather:localhost";
    ch.registered_users.write().await.insert(sender.to_string());

    let mut first_event_id = None;
    for idx in 0..=MAX_EVENT_SENDER_CACHE {
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: format!("message {idx}"),
            reply_to: None,
            media: vec![],
            metadata: json!({ METADATA_SENDER_USER_ID: sender }),
        };
        let event_id = ch.send_with_id(&msg).await.unwrap().unwrap();
        if idx == 0 {
            first_event_id = Some(event_id);
        }
    }

    let senders = ch.event_senders.read().await;
    assert_eq!(senders.len(), MAX_EVENT_SENDER_CACHE);
    assert!(
        !senders
            .iter()
            .any(|(event_id, _)| Some(event_id) == first_event_id.as_ref()),
        "oldest event sender entry should be evicted when cache exceeds the bound"
    );

    handle.abort();
}

#[test]
fn test_reload_error_response_escapes_json() {
    let (status, axum::Json(body)) =
        error_json_response(StatusCode::INTERNAL_SERVER_ERROR, "bad \"quote\"");

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({ "error": "bad \"quote\"" }));
}

// ── Registration YAML tests ─────────────────────────────────────────

#[test]
fn test_matrix_generate_registration_yaml() {
    let ch = MatrixChannel::new(
        "http://localhost:6167",
        "test-as-token",
        "test-hs-token",
        "localhost",
        "bot",
        "bot_",
        8009,
        Arc::new(AtomicBool::new(false)),
    );

    let tmp = tempfile::tempdir().unwrap();
    let path = ch.generate_registration(tmp.path()).unwrap();

    assert!(path.exists());
    assert_eq!(
        path.file_name().unwrap(),
        "matrix-appservice-registration.yaml"
    );

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("id:"), "missing id field");
    assert!(
        content.contains("as_token: test-as-token"),
        "missing as_token"
    );
    assert!(
        content.contains("hs_token: test-hs-token"),
        "missing hs_token"
    );
    assert!(
        content.contains("sender_localpart: bot"),
        "missing sender_localpart"
    );
    assert!(
        content.contains("url: http://localhost:8009"),
        "missing url"
    );
    assert!(
        content.contains("@bot_.*:localhost"),
        "missing user namespace regex"
    );
    assert!(content.contains("namespaces:"), "missing namespaces");
}

#[test]
fn test_matrix_registration_no_overwrite() {
    let ch = make_channel();
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("matrix-appservice-registration.yaml");

    // Write a custom file first
    std::fs::write(&file_path, "custom").unwrap();

    let returned_path = ch.generate_registration(tmp.path()).unwrap();
    assert_eq!(returned_path, file_path);

    let content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "custom", "existing file should not be overwritten");
}

#[test]
fn test_matrix_registration_parseable() {
    let ch = make_channel();
    let tmp = tempfile::tempdir().unwrap();
    let path = ch.generate_registration(tmp.path()).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_yml::from_str(&content).unwrap();

    assert!(
        parsed.get("as_token").is_some(),
        "parsed YAML missing as_token"
    );
    assert!(
        parsed.get("hs_token").is_some(),
        "parsed YAML missing hs_token"
    );
    assert!(
        parsed.get("sender_localpart").is_some(),
        "parsed YAML missing sender_localpart"
    );
    assert!(
        parsed.get("namespaces").is_some(),
        "parsed YAML missing namespaces"
    );
}

// ── BotRouter tests ─────────────────────────────────────────────────

#[tokio::test]
async fn test_bot_router_register_and_route() {
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather-001")
        .await
        .unwrap();
    let result = router.route("@bot_weather:localhost").await;
    assert_eq!(result, Some("profile-weather-001".to_string()));
}

#[tokio::test]
async fn test_bot_router_unknown_returns_none() {
    let router = BotRouter::new(None);
    let result = router.route("@bot_unknown:localhost").await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_bot_router_unregister() {
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-001")
        .await
        .unwrap();
    router.unregister("@bot_weather:localhost").await.unwrap();
    let result = router.route("@bot_weather:localhost").await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_bot_router_persistence() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("matrix-bot-routes.json");

    // Register a mapping and persist
    {
        let router = BotRouter::new(Some(path.clone()));
        router
            .register("@bot_a:localhost", "profile-a")
            .await
            .unwrap();
    }

    // Create a new router from the same path and verify it loaded
    let router2 = BotRouter::new(Some(path));
    let result = router2.route("@bot_a:localhost").await;
    assert_eq!(result, Some("profile-a".to_string()));
}

#[test]
fn test_bot_visibility_serializes_lowercase() {
    let public_json = serde_json::to_string(&BotVisibility::Public).unwrap();
    assert_eq!(public_json, "\"public\"");

    let private_json = serde_json::to_string(&BotVisibility::Private).unwrap();
    assert_eq!(private_json, "\"private\"");
}

#[tokio::test]
async fn test_bot_router_loads_old_format() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("matrix-bot-routes.json");
    std::fs::write(&path, r#"{"@bot_weather:localhost":"main--weather"}"#).unwrap();

    let router = BotRouter::new(Some(path));
    let entry = router
        .get_entry("@bot_weather:localhost")
        .await
        .expect("legacy route should load");

    assert_eq!(entry.profile_id, "main--weather");
    assert_eq!(entry.owner, "");
    assert_eq!(entry.visibility, BotVisibility::Public);
}

#[tokio::test]
async fn test_bot_router_loads_new_format() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("matrix-bot-routes.json");
    std::fs::write(
            &path,
            r#"{"@bot_weather:localhost":{"profile_id":"main--weather","owner":"@alice:localhost","visibility":"public"}}"#,
        )
        .unwrap();

    let router = BotRouter::new(Some(path));
    let entry = router
        .get_entry("@bot_weather:localhost")
        .await
        .expect("new-format route should load");

    assert_eq!(entry.profile_id, "main--weather");
    assert_eq!(entry.owner, "@alice:localhost");
    assert_eq!(entry.visibility, BotVisibility::Public);
}

#[tokio::test]
async fn test_matrix_register_bot_registers_user_and_route() {
    let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        9880,
        Arc::new(AtomicBool::new(false)),
    );

    ch.register_bot("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();

    assert_eq!(
        ch.bot_router().route("@octos_weather:localhost").await,
        Some("profile-weather".to_string())
    );

    wait_for_request_count(&requests, 1).await;
    let requests = requests.lock().await;
    assert!(
        requests
            .iter()
            .any(|req| req.path == "/_matrix/client/v3/register")
    );

    homeserver_handle.abort();
}

#[tokio::test]
async fn test_matrix_unregister_bot_removes_route() {
    let ch = make_channel();
    ch.bot_router()
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();

    ch.unregister_bot("@octos_weather:localhost").await.unwrap();

    assert_eq!(
        ch.bot_router().route("@octos_weather:localhost").await,
        None
    );
}

#[tokio::test]
async fn test_matrix_unregister_bot_removes_registered_sender() {
    let ch = make_channel();
    {
        let mut users = ch.registered_users.write().await;
        users.insert("@octos_weather:localhost".to_string());
    }
    ch.bot_router()
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();

    ch.unregister_bot("@octos_weather:localhost").await.unwrap();

    let users = ch.registered_users.read().await;
    assert!(
        !users.contains("@octos_weather:localhost"),
        "unregister_bot should remove sender authorization"
    );
}

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

/// Regression test: after gateway restart, bots persisted in the route map
/// must be restored into `registered_users` so outbound sends succeed.
#[tokio::test]
async fn test_startup_restores_registered_users_from_persisted_routes() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
    let tmp = tempfile::tempdir().unwrap();

    // Pre-populate a routes file (simulating a prior gateway session)
    let routes_path = tmp.path().join("matrix-bot-routes.json");
    std::fs::write(
        &routes_path,
        r#"{"@octos_weather:localhost":"profile-weather"}"#,
    )
    .unwrap();

    let ch = MatrixChannel::new(
        &homeserver,
        "as_token_test",
        "hs_token_test",
        "localhost",
        "octos_bot",
        "octos_",
        unused_local_port(),
        Arc::new(AtomicBool::new(false)),
    )
    .with_bot_router(tmp.path());

    // Before start: registered_users should be empty
    assert!(
        !ch.registered_users
            .read()
            .await
            .contains("@octos_weather:localhost"),
        "bot should not be in registered_users before start"
    );

    // Simulate what start() does: register bot user + restore from routes
    {
        let mut users = ch.registered_users.write().await;
        users.insert(ch.bot_user_id.clone());
        for (matrix_user_id, _) in ch.bot_router.list_routes().await {
            users.insert(matrix_user_id);
        }
    }

    // After restore: the persisted bot should be in registered_users
    assert!(
        ch.registered_users
            .read()
            .await
            .contains("@octos_weather:localhost"),
        "persisted bot must be restored into registered_users on startup"
    );

    // Outbound send should succeed (not rejected as unregistered)
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "Hello from restored bot".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({"sender_user_id": "@octos_weather:localhost"}),
    };
    ch.send_with_id(&msg)
        .await
        .expect("send should succeed for restored bot");

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let send_req = reqs.iter().find(|r| r.path.contains("/send/")).unwrap();
    let query = send_req.query.as_deref().unwrap_or("");
    assert!(
        query.contains("user_id=%40octos_weather%3Alocalhost"),
        "send should use the restored bot identity, got query: {query}"
    );

    handle.abort();
}

#[tokio::test]
async fn test_bot_router_no_metadata_without_mapping() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    // State with an empty bot router (no mappings)
    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room123:localhost",
            "event_id": "$ev_no_route",
            "content": {
                "msgtype": "m.text",
                "body": "hello, no bot mentioned here"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_no_route?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert!(
        msg.metadata.get(METADATA_TARGET_PROFILE_ID).is_none(),
        "metadata should not contain target_profile_id when no bot mapping exists"
    );
}

#[tokio::test]
async fn test_bot_router_injects_metadata() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let mut state = make_test_state(inbound_tx);

    // Register a bot mapping in the router
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room123:localhost",
            "event_id": "$ev_routed",
            "content": {
                "msgtype": "m.text",
                "body": "Hey @bot_weather:localhost what is the forecast?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_routed?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "metadata should contain the routed profile_id"
    );
}

#[tokio::test]
async fn test_bot_router_does_not_match_user_id_substrings() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

    let mut state = make_test_state(inbound_tx);
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:elsewhere.org",
            "room_id": "!room123:localhost",
            "event_id": "$ev_substring",
            "content": {
                "msgtype": "m.text",
                "body": "Hey @bot_weather:localhost123 are you there?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_substring?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert!(
        msg.metadata.get(METADATA_TARGET_PROFILE_ID).is_none(),
        "substring matches should not route to a bot profile"
    );
}

// ── Track A: sender_user_id tests ──────────────────────────────────────

#[tokio::test]
async fn test_matrix_send_with_sender_user_id() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    // Register the virtual user so it's allowed
    ch.registered_users
        .write()
        .await
        .insert("@octos_weather:localhost".to_string());

    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "Hello from weather bot".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({"sender_user_id": "@octos_weather:localhost"}),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let send_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    let query = send_req.query.as_deref().unwrap_or("");
    assert!(
        query.contains("user_id=%40octos_weather%3Alocalhost"),
        "URL should use sender_user_id from metadata, got query: {query}"
    );

    handle.abort();
}

#[tokio::test]
async fn test_matrix_send_default_sender() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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

    // No sender_user_id in metadata → should use default bot_user_id
    let msg = OutboundMessage {
        channel: "matrix".to_string(),
        chat_id: "!room:localhost".to_string(),
        content: "Hello from default bot".to_string(),
        reply_to: None,
        media: vec![],
        metadata: json!({}),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let send_req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    let query = send_req.query.as_deref().unwrap_or("");
    assert!(
        query.contains("user_id=%40octos_bot%3Alocalhost"),
        "URL should use default bot_user_id when sender_user_id is absent, got query: {query}"
    );

    handle.abort();
}

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
async fn test_bot_router_add_room_bot_and_route_by_room() {
    let router = BotRouter::new(None);
    router
        .add_room_bot("!dm1:localhost", "profile-weather")
        .await
        .unwrap();

    let result = router.route_by_room("!dm1:localhost").await;
    assert_eq!(result, Some("profile-weather".to_string()));
}

#[tokio::test]
async fn test_bot_router_route_by_room_multi_bot_returns_none() {
    let router = BotRouter::new(None);
    router
        .add_room_bot("!group:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!group:localhost", "profile-news")
        .await
        .unwrap();

    // Multiple bots in room → require @mention
    let result = router.route_by_room("!group:localhost").await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_bot_router_route_by_room_unknown_room() {
    let router = BotRouter::new(None);
    let result = router.route_by_room("!unknown:localhost").await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_bot_router_remove_bot_from_rooms() {
    let router = BotRouter::new(None);
    router
        .add_room_bot("!dm1:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!dm2:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!dm2:localhost", "profile-news")
        .await
        .unwrap();

    router
        .remove_bot_from_rooms("profile-weather")
        .await
        .unwrap();

    assert_eq!(router.route_by_room("!dm1:localhost").await, None);
    // dm2 still has profile-news
    assert_eq!(
        router.route_by_room("!dm2:localhost").await,
        Some("profile-news".to_string())
    );
}

#[tokio::test]
async fn test_bot_router_room_map_persistence() {
    let tmp = tempfile::tempdir().unwrap();
    let routes_path = tmp.path().join("matrix-bot-routes.json");

    // Create router, add room mapping
    {
        let router = BotRouter::new(Some(routes_path.clone()));
        router
            .add_room_bot("!dm1:localhost", "profile-weather")
            .await
            .unwrap();
    }

    // New router from same path should load room mappings
    let router2 = BotRouter::new(Some(routes_path));
    assert_eq!(
        router2.route_by_room("!dm1:localhost").await,
        Some("profile-weather".to_string())
    );
}

#[tokio::test]
async fn test_unregister_bot_cleans_room_mappings() {
    let ch = make_channel();
    ch.bot_router()
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    ch.bot_router()
        .add_room_bot("!dm1:localhost", "profile-weather")
        .await
        .unwrap();

    // Add to registered_users so unregister_bot can clean up
    ch.registered_users
        .write()
        .await
        .insert("@octos_weather:localhost".to_string());

    ch.unregister_bot("@octos_weather:localhost").await.unwrap();

    // Room mapping should be cleaned up
    assert_eq!(ch.bot_router().route_by_room("!dm1:localhost").await, None);
    // User route should also be gone
    assert_eq!(
        ch.bot_router().route("@octos_weather:localhost").await,
        None
    );
}

#[tokio::test]
async fn test_handle_transaction_dm_routing() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

    // Set up a room-bot mapping (simulate bot already joined DM room)
    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!dm_room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    // Send a message WITHOUT @mention in the DM room
    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!dm_room:localhost",
            "event_id": "$dm1",
            "content": {
                "msgtype": "m.text",
                "body": "What's the weather today?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-dm-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "DM message should route to weather bot via room mapping"
    );
}

#[tokio::test]
async fn test_handle_transaction_mention_gate_blocks_unaddressed_group_room_mapping() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (homeserver, requests, homeserver_handle) =
        spawn_mock_homeserver_with_joined_members(json!({
            "joined": {
                "@octos_bot:localhost": {},
                "@octos_weather:localhost": {},
                "@alice:localhost": {},
                "@bob:localhost": {}
            }
        }))
        .await;
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.mention_only = true;

    let router = BotRouter::new(None);
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!group_room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!group_room:localhost",
            "event_id": "$gate-group-1",
            "content": {
                "msgtype": "m.text",
                "body": "What's the weather today?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-gate-group?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        inbound_rx.try_recv().is_err(),
        "unaddressed group room-mapping fallback should not reach the agent"
    );
    wait_for_request_count(&requests, 1).await;
    let requests = requests.lock().await;
    assert!(
        requests
            .iter()
            .any(|req| req.path.contains("/joined_members")),
        "mention gate should derive DM-ness from live room membership"
    );
    homeserver_handle.abort();
}

#[tokio::test]
async fn test_handle_transaction_mention_gate_allows_unaddressed_one_to_one_dm() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (homeserver, _requests, homeserver_handle) =
        spawn_mock_homeserver_with_joined_members(json!({
            "joined": {
                "@octos_weather:localhost": {},
                "@alice:localhost": {}
            }
        }))
        .await;
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.mention_only = true;

    let router = BotRouter::new(None);
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!dm_room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!dm_room:localhost",
            "event_id": "$gate-dm-1",
            "content": {
                "msgtype": "m.text",
                "body": "What's the weather today?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-gate-dm?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "1:1 DM should continue to route through room mapping without an explicit mention"
    );
    homeserver_handle.abort();
}

#[tokio::test]
async fn gate_blocks_unaddressed_when_room_map_has_multiple_bots() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // Palpo hides appservice virtual users: joined_members shows ONLY the
    // human, yet the room map knows two bots are in this room. The
    // unaddressed message must not reach any agent.
    let (homeserver, _requests, homeserver_handle) =
        spawn_mock_homeserver_with_joined_members(json!({
            "joined": {
                "@alice:localhost": {}
            }
        }))
        .await;
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.mention_only = true;

    let router = BotRouter::new(None);
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .register("@octos_translator:localhost", "profile-translator")
        .await
        .unwrap();
    router
        .add_room_bot("!multi_bot_room:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!multi_bot_room:localhost", "profile-translator")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!multi_bot_room:localhost",
            "event_id": "$gate-multibot-1",
            "content": {
                "msgtype": "m.text",
                "body": "hello everyone"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-gate-multibot?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        inbound_rx.try_recv().is_err(),
        "a room with two mapped bots is not a DM: unaddressed messages must be gated even when the homeserver hides bot members"
    );
    homeserver_handle.abort();
}

#[tokio::test]
async fn gate_allows_unaddressed_dm_with_single_mapped_bot() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // Same homeserver blindness (only the human in joined_members), but
    // the room map has exactly one bot: this IS a 1:1 agent DM and the
    // exemption applies.
    let (homeserver, _requests, homeserver_handle) =
        spawn_mock_homeserver_with_joined_members(json!({
            "joined": {
                "@alice:localhost": {}
            }
        }))
        .await;
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.homeserver = homeserver;
    state.mention_only = true;

    let router = BotRouter::new(None);
    router
        .register("@octos_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .add_room_bot("!hidden_dm_room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!hidden_dm_room:localhost",
            "event_id": "$gate-hidden-dm-1",
            "content": {
                "msgtype": "m.text",
                "body": "What's the weather today?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-gate-hidden-dm?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "a 1:1 agent DM replies without a mention even when the homeserver hides the bot member"
    );
    homeserver_handle.abort();
}

#[tokio::test]
async fn test_handle_transaction_member_event_invalidates_dm_member_cache() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);
    state.dm_member_cache.write().await.insert(
        "!room:localhost".to_string(),
        (membership(1, 1, false), Instant::now()),
    );
    let cache = state.dm_member_cache.clone();

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.member",
            "sender": "@alice:localhost",
            "state_key": "@octos_translator:localhost",
            "room_id": "!room:localhost",
            "content": {
                "membership": "join"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-member-cache?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !cache.read().await.contains_key("!room:localhost"),
        "membership changes must force the mention gate to refresh room counts"
    );
}

#[tokio::test]
async fn test_private_bot_message_blocked_for_non_owner() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

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

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-private-msg?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
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

#[tokio::test]
async fn test_private_bot_rejection_sent_when_explicitly_addressed() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@mallory:localhost",
            "room_id": "!private:localhost",
            "event_id": "$private-addressed-1",
            "content": {
                "msgtype": "m.text",
                "body": "hey @octos_private:localhost help me",
                "m.mentions": { "user_ids": ["@octos_private:localhost"] }
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-private-addressed?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        inbound_rx.try_recv().is_err(),
        "non-owner message should not be forwarded to the agent"
    );

    // The sender explicitly addressed the private bot, so the rejection
    // reply is warranted feedback rather than unsolicited noise.
    wait_for_request_count(&requests, 1).await;
    let requests = requests.lock().await;
    assert!(requests.iter().any(|req| {
        req.path.contains("/send/")
            && req
                .query
                .as_deref()
                .is_some_and(|q| q.contains("user_id=%40octos_private%3Alocalhost"))
    }));

    homeserver_handle.abort();
}

#[tokio::test]
async fn test_private_bot_message_allowed_for_owner() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

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

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@owner:localhost",
            "room_id": "!private:localhost",
            "event_id": "$private-owner",
            "content": {
                "msgtype": "m.text",
                "body": "hello private bot"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-private-owner?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx
        .try_recv()
        .expect("owner message should be forwarded");
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("main--private")
    );
}

#[tokio::test]
async fn test_handle_transaction_mention_priority() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .register("@bot_news:localhost", "profile-news")
        .await
        .unwrap();
    // Room is mapped to weather bot
    router
        .add_room_bot("!room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    // Message mentions news bot, even though room is mapped to weather
    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$mention1",
            "content": {
                "msgtype": "m.text",
                "body": "@bot_news:localhost what's the latest?"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-mention-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-news"),
        "@mention should take priority over room mapping"
    );
}

#[tokio::test]
async fn test_handle_transaction_m_mentions_routes_to_target_bot() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

    let router = BotRouter::new(None);
    router
        .register("@octos_mybot:localhost", "profile-mybot")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$mentions1",
            "content": {
                "msgtype": "m.text",
                "body": "mybot: 你又是谁",
                "m.mentions": {
                    "user_ids": ["@octos_mybot:localhost"]
                }
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-mentions-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-mybot"),
        "m.mentions user_ids should route to the selected bot"
    );
}

#[tokio::test]
async fn test_handle_transaction_explicit_target_user_id() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$explicit1",
            "content": {
                "msgtype": "m.text",
                "body": "What's the weather today?",
                "org.octos.target_user_id": "@bot_weather:localhost"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-explicit-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "explicit target_user_id should route to the selected bot"
    );
}

#[tokio::test]
async fn test_handle_transaction_copies_action_response_into_metadata() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$action-response-1",
            "content": {
                "msgtype": "m.text",
                "body": "ack",
                "org.octos.action_response": {
                    "action_id": "ack",
                    "app_id": "mission:alpha",
                    "state": { "acknowledged": true }
                }
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-action-response?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata[CONTENT_ACTION_RESPONSE]["state"]["acknowledged"],
        json!(true)
    );
}

#[tokio::test]
async fn test_handle_transaction_explicit_target_user_id_priority() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);

    let router = BotRouter::new(None);
    router
        .register("@bot_weather:localhost", "profile-weather")
        .await
        .unwrap();
    router
        .register("@bot_news:localhost", "profile-news")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-weather")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$explicit2",
            "content": {
                "msgtype": "m.text",
                "body": "@bot_news:localhost what's the weather today?",
                "org.octos.target_user_id": "@bot_weather:localhost"
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-explicit-2?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msg = inbound_rx.try_recv().unwrap();
    assert_eq!(
        msg.metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str()),
        Some("profile-weather"),
        "explicit target_user_id should take priority over mention and room routing"
    );
}

// ── Slash command parsing tests ──────────────────────────────────

#[test]
fn test_extract_prompt_flag_no_prompt() {
    let (args, prompt) = extract_prompt_flag("weather Weather Bot");
    assert_eq!(args, "weather Weather Bot");
    assert!(prompt.is_none());
}

#[test]
fn test_extract_prompt_flag_quoted() {
    let (args, prompt) = extract_prompt_flag("weather Weather Bot --prompt \"你是天气助手\"");
    assert_eq!(args, "weather Weather Bot");
    assert_eq!(prompt.as_deref(), Some("你是天气助手"));
}

#[test]
fn test_extract_prompt_flag_unquoted() {
    let (args, prompt) = extract_prompt_flag("weather --prompt simple prompt text");
    assert_eq!(args, "weather");
    assert_eq!(prompt.as_deref(), Some("simple prompt text"));
}

#[test]
fn test_extract_prompt_flag_empty_prompt() {
    let (args, prompt) = extract_prompt_flag("weather --prompt");
    assert_eq!(args, "weather");
    assert!(prompt.is_none());
}

#[test]
fn test_extract_visibility_flag_public() {
    let (args, visibility) =
        extract_visibility_flag("weather Weather Bot --public --prompt \"hello\"");
    assert_eq!(args, "weather Weather Bot --prompt \"hello\"");
    assert_eq!(visibility, Some(BotVisibility::Public));
}

#[test]
fn test_extract_visibility_flag_private() {
    let (args, visibility) = extract_visibility_flag("weather Weather Bot --private");
    assert_eq!(args, "weather Weather Bot");
    assert_eq!(visibility, Some(BotVisibility::Private));
}

#[test]
fn test_extract_visibility_flag_missing() {
    let (args, visibility) = extract_visibility_flag("weather Weather Bot");
    assert_eq!(args, "weather Weather Bot");
    assert_eq!(visibility, None);
}

#[tokio::test]
async fn test_slash_command_not_intercepted_without_bot_manager() {
    let (tx, _rx) = mpsc::channel(1);
    let state = make_test_state(tx);
    // bot_manager is None, so slash commands should not be intercepted
    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/listbots",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_slash_command_not_intercepted_for_normal_messages() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "hello world",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_slash_command_listbots() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/listbots",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    assert!(result.unwrap().contains("mock list"));
}

#[tokio::test]
async fn test_slash_command_createbot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/createbot weather Weather Bot --prompt \"你是天气助手\"",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    let msg = result.unwrap();
    assert!(msg.contains("mock create"), "got: {msg}");
}

#[tokio::test]
async fn test_slash_command_createbot_defaults_private() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(RecordingBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/createbot weather Weather Bot",
        None,
        &json!({}),
        None,
    )
    .await;

    let msg = result.expect("createbot should be intercepted");
    assert!(msg.contains("mock create"), "got: {msg}");
    assert!(
        msg.contains("Private"),
        "expected default private visibility: {msg}"
    );
}

#[tokio::test]
async fn test_slash_command_createbot_with_public_visibility() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(RecordingBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/createbot weather Weather Bot --public",
        None,
        &json!({}),
        None,
    )
    .await;

    let msg = result.expect("createbot should be intercepted");
    assert!(msg.contains("mock create"), "got: {msg}");
    assert!(msg.contains("Public"), "expected public visibility: {msg}");
}

#[tokio::test]
async fn test_slash_command_deletebot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/deletebot @bot_weather:localhost",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    assert!(result.unwrap().contains("mock delete"));
}

#[tokio::test]
async fn test_slash_command_createbot_missing_args() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/createbot",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    assert!(result.unwrap().contains("Usage"));
}

#[tokio::test]
async fn test_slash_command_deletebot_missing_args() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/deletebot",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    assert!(result.unwrap().contains("Usage"));
}

#[tokio::test]
async fn test_slash_command_listbot_singular_alias() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/listbot",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(result.is_some());
    assert!(result.unwrap().contains("mock list"));
}

#[tokio::test]
async fn test_slash_command_unknown_command_not_intercepted() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/unknown",
        None,
        &json!({}),
        None,
    )
    .await;
    assert!(
        result.is_none(),
        "unknown slash commands should pass through to agent"
    );
}

#[tokio::test]
async fn should_not_intercept_slash_command_when_aimed_at_child_bot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/schedule 每天早上 9 点提醒我看天气",
        Some("@octos_child:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert!(
        result.is_none(),
        "slash commands aimed at a child bot must flow through to that bot"
    );
}

#[tokio::test]
async fn should_intercept_schedule_command_when_aimed_at_primary_bot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/schedule 每天早上 9 点提醒我看天气",
        Some("@octos_bot:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert_eq!(
        result,
        Some("mock schedule: 每天早上 9 点提醒我看天气 in !room:localhost".to_string()),
    );
}

#[tokio::test]
async fn should_intercept_schedules_command_when_aimed_at_primary_bot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/schedules",
        Some("@octos_bot:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert_eq!(
        result,
        Some("mock schedules for !room:localhost".to_string())
    );
}

#[tokio::test]
async fn should_intercept_unschedule_command_when_aimed_at_primary_bot() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/unschedule cron_deadbeef",
        Some("@octos_bot:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert_eq!(
        result,
        Some("mock unschedule: cron_deadbeef in !room:localhost".to_string()),
    );
}

#[tokio::test]
async fn should_require_message_body_when_allbots_invoked() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/allbots",
        Some("@octos_bot:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert_eq!(result, Some("Usage: `/allbots <message>`".to_string()));
}

#[tokio::test]
async fn should_reject_allbots_when_no_broadcast_targets() {
    let (tx, _rx) = mpsc::channel(1);
    let mut state = make_test_state(tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let result = handle_slash_command(
        &state,
        "@alice:localhost",
        "!room:localhost",
        "/allbots summarize this issue",
        Some("@octos_bot:localhost"),
        &json!({}),
        None,
    )
    .await;

    assert_eq!(
        result,
        Some("No bound child bots were found for this room.".to_string()),
    );
}

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
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    router
        .add_room_bot("!room:localhost", "profile-alex")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-bob")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

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
                    "@octos_bob:localhost"
                ]
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-allbots-fanout-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
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
        "/allbots should internally fan out to the bound child bots",
    );
}

#[tokio::test]
async fn should_skip_stale_bindings_when_allbots_fans_out() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let router = BotRouter::new(None);
    router
        .register("@octos_bot:localhost", "profile-parent")
        .await
        .unwrap();
    router
        .register("@octos_alexbot:localhost", "profile-alex")
        .await
        .unwrap();
    router
        .add_room_bot("!room:localhost", "profile-alex")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    // "@octos_alex:localhost" has no router entry — a stale binding.
    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$allbots-fanout-stale-1",
            "content": {
                "msgtype": "m.text",
                "body": "/allbots summarize this issue",
                "org.octos.target_user_id": "@octos_bot:localhost",
                "org.octos.broadcast_targets": [
                    "@octos_alex:localhost",
                    "@octos_alexbot:localhost"
                ]
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-allbots-fanout-stale-1?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
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

    assert_eq!(
        seen,
        vec![(
            "summarize this issue".to_string(),
            Some("profile-alex".to_string()),
        )],
        "/allbots should skip stale bindings and still fan out to valid child bots",
    );
}

#[test]
fn should_reject_outbound_media_file_over_cap() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("big.bin");
    std::fs::write(&file, vec![0u8; 2048]).unwrap();

    // 2048-byte file, cap = 1024 → rejected.
    let err = check_upload_within_cap(&file, 1024).unwrap_err();
    assert!(
        err.to_string().contains("exceeds max upload size"),
        "got: {err}"
    );

    // Same file, cap = 4096 → accepted, returns the size.
    assert_eq!(check_upload_within_cap(&file, 4096).unwrap(), 2048);
}

#[tokio::test]
async fn should_reject_allbots_targets_not_bound_to_this_room() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let mut state = make_test_state(inbound_tx);
    state.bot_manager = Some(Arc::new(MockBotManager));

    let router = BotRouter::new(None);
    router
        .register("@octos_bot:localhost", "profile-parent")
        .await
        .unwrap();
    // A public bot that exists globally but is bound to a DIFFERENT room.
    router
        .register("@octos_elsewhere:localhost", "profile-elsewhere")
        .await
        .unwrap();
    router
        .add_room_bot("!other:localhost", "profile-elsewhere")
        .await
        .unwrap();
    state.bot_router = Arc::new(router);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    // Forged event tries to broadcast to a bot bound to "!other:localhost"
    // from within "!room:localhost".
    let body = serde_json::json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$allbots-cross-room",
            "content": {
                "msgtype": "m.text",
                "body": "/allbots leak to other room",
                "org.octos.target_user_id": "@octos_bot:localhost",
                "org.octos.broadcast_targets": ["@octos_elsewhere:localhost"]
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn-allbots-cross-room?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // No broadcast should have been dispatched to the cross-room bot.
    let mut dispatched = Vec::new();
    while let Ok(msg) = inbound_rx.try_recv() {
        if let Some(p) = msg
            .metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|v| v.as_str())
        {
            dispatched.push(p.to_string());
        }
    }
    assert!(
        !dispatched.iter().any(|p| p == "profile-elsewhere"),
        "/allbots must not fan out to a bot bound to a different room: {dispatched:?}"
    );
}

#[tokio::test]
async fn should_project_approval_request_into_event_content() {
    let (homeserver, requests, handle) = spawn_mock_homeserver().await;
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
    let msg = OutboundMessage {
        channel: "matrix".into(),
        chat_id: "!room:localhost".into(),
        content: "Approval required: Approve shell command".into(),
        reply_to: None,
        media: vec![],
        metadata: json!({
            CONTENT_APPROVAL_REQUEST: {
                "request_id": "req_1_1",
                "tool_name": "shell",
                "tool_args_digest": "sha256:abc",
                "title": "Approve shell command",
                "summary": "rm -rf tmp",
                "risk_level": "critical",
                "authorized_approvers": ["@alice:localhost"],
                "expires_at": "2026-06-13T12:00:00Z",
                "on_timeout": "notify"
            },
            CONTENT_ACTIONS: [
                { "id": "approve", "label": "Approve", "style": "primary" },
                { "id": "deny", "label": "Deny", "style": "danger" }
            ]
        }),
    };

    ch.send_with_id(&msg).await.unwrap();

    wait_for_request_count(&requests, 1).await;
    let reqs = requests.lock().await;
    let req = reqs
        .iter()
        .find(|r| r.path.contains("/send/"))
        .expect("should have a send request");
    assert_eq!(
        req.body[CONTENT_APPROVAL_REQUEST]["request_id"],
        json!("req_1_1")
    );
    assert_eq!(
        req.body[CONTENT_APPROVAL_REQUEST]["tool_name"],
        json!("shell")
    );
    assert_eq!(req.body[CONTENT_ACTIONS][0]["id"], json!("approve"));

    handle.abort();
}

#[tokio::test]
async fn should_copy_approval_response_into_inbound_metadata() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
    let state = make_test_state(inbound_tx);

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transaction),
        )
        .with_state(state);

    let body = json!({
        "events": [{
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "room_id": "!room:localhost",
            "event_id": "$approval_resp",
            "content": {
                "msgtype": "m.text",
                "body": "approve",
                "org.octos.approval_response": {
                    "request_id": "req_1_1",
                    "decision": "approve",
                    "source_event_id": "$approval_resp",
                    "tool_args_digest": "sha256:abc"
                }
            }
        }]
    });

    let req = Request::builder()
        .method("PUT")
        .uri("/_matrix/app/v1/transactions/txn_approval?access_token=test_token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let inbound = inbound_rx.recv().await.expect("inbound message");
    assert_eq!(
        inbound.metadata[CONTENT_APPROVAL_RESPONSE]["request_id"],
        json!("req_1_1")
    );
    assert_eq!(
        inbound.metadata[CONTENT_APPROVAL_RESPONSE]["decision"],
        json!("approve")
    );
}

/// Mock BotManager for testing slash command dispatch.
struct MockBotManager;

#[derive(Default)]
struct RecordingBotManager;

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

#[async_trait]
impl BotManager for RecordingBotManager {
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
