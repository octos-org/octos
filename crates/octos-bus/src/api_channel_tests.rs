use super::*;
use axum::body::Body;
use axum::http::Request;
use std::path::Path;
use tower::util::ServiceExt;

#[test]
fn effective_upload_tenant_is_never_none_and_matches_upload_stamp() {
    // Cross-profile target (charset-valid) wins.
    assert_eq!(
        effective_upload_tenant(Some("dspfac--lbc-bot"), Some("dspfac")),
        "dspfac--lbc-bot"
    );
    // Invalid target (path-injection / uppercase / empty) is ignored,
    // falls back to the gateway profile.
    assert_eq!(
        effective_upload_tenant(Some("../etc"), Some("dspfac")),
        "dspfac"
    );
    assert_eq!(effective_upload_tenant(Some(""), Some("dspfac")), "dspfac");
    assert_eq!(
        effective_upload_tenant(Some("Dspfac"), Some("dspfac")),
        "dspfac"
    );
    // No target -> gateway profile.
    assert_eq!(effective_upload_tenant(None, Some("dspfac")), "dspfac");
    // No target AND no gateway profile (main/admin gateway) -> `_main`,
    // NEVER None — so the download gate actually fires (codex pre-merge P1).
    assert_eq!(
        effective_upload_tenant(None, None),
        octos_core::MAIN_PROFILE_ID
    );
    assert_eq!(
        effective_upload_tenant(Some(""), None),
        octos_core::MAIN_PROFILE_ID
    );
}

const TEST_PROFILE_ID: &str = "dspfac";

/// M8.10 PR #2: the synthetic SSE message_id round-trips through
/// encode → decode without losing the bound thread_id. This is the
/// thread that lets `edit_message` recover the cmid for its
/// streaming `token`/`replace` payloads.
#[test]
fn sse_message_id_roundtrips_chat_id_and_thread_id() {
    let encoded = encode_sse_message_id("chat-A", Some("cmid-T-1"));
    let (chat, tid) = decode_sse_message_id(&encoded);
    assert_eq!(chat, "sse-chat-A");
    assert_eq!(tid, Some("cmid-T-1"));
}

#[test]
fn sse_message_id_omits_thread_id_when_unbound() {
    let encoded = encode_sse_message_id("chat-A", None);
    assert_eq!(encoded, "sse-chat-A");
    let (chat, tid) = decode_sse_message_id(&encoded);
    assert_eq!(chat, "sse-chat-A");
    assert_eq!(tid, None);
}

#[test]
fn outbound_thread_id_extracts_string_from_metadata() {
    let m = serde_json::json!({"thread_id": "cmid-T"});
    assert_eq!(outbound_thread_id(&m).as_deref(), Some("cmid-T"));
}

#[test]
fn outbound_thread_id_treats_empty_string_as_absent() {
    let m = serde_json::json!({"thread_id": ""});
    assert!(outbound_thread_id(&m).is_none());
}

#[test]
fn outbound_thread_id_returns_none_when_absent() {
    let m = serde_json::json!({});
    assert!(outbound_thread_id(&m).is_none());
}

fn test_sessions_in(data_dir: &Path) -> Arc<Mutex<SessionManager>> {
    Arc::new(Mutex::new(SessionManager::open(data_dir).unwrap()))
}

fn test_sessions() -> Arc<Mutex<SessionManager>> {
    let dir = std::env::temp_dir().join(format!("octos-bus-tests-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    test_sessions_in(&dir)
}

fn assistant_tool_call_message(tool_name: &str, arguments: serde_json::Value) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![octos_core::ToolCall {
            id: format!("call-{tool_name}"),
            name: tool_name.to_string(),
            arguments,
            metadata: None,
        }]),
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        // PR F (M8.10): pre-stamp thread_id for the test helper so the
        // new-write fail-closed split accepts it. Production code uses
        // `Message::assistant_with_thread`.
        thread_id: Some("test-thread".to_string()),
        timestamp: Utc::now(),
    }
}

fn test_ui_sink() -> UiEventSink {
    UiEventSink::new(Arc::new(StdMutex::new(HashMap::new())))
}

fn test_turn_context(session_id: &str, thread_id: &str) -> TurnContext {
    TurnContext::new(session_id.to_string(), None, ThreadId::new(thread_id))
}

#[test]
fn chat_request_deserialize() {
    let json = r#"{"message": "hello"}"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.message, "hello");
    assert!(req.session_id.is_none());
    assert!(req.topic.is_none());
}

#[test]
fn chat_request_deserialize_with_topic() {
    let json = r#"{"message": "hello", "session_id": "slides-123", "topic": "slides untitled"}"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.message, "hello");
    assert_eq!(req.session_id.as_deref(), Some("slides-123"));
    assert_eq!(req.topic.as_deref(), Some("slides untitled"));
}

#[test]
fn chat_request_with_session() {
    let json = r#"{"message": "hi", "session_id": "web-123"}"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.session_id.as_deref(), Some("web-123"));
}

/// FA-12f/M9 regression: POST /api/chat may carry the legacy
/// `client_message_id` alias, which must survive as canonical thread_id and
/// `InboundMessage.message_id` so downstream overflow emission can
/// propagate it into `_session_result.response_to_client_message_id`
/// — the field the web reducer correlates against the optimistic
/// streaming bubble.
///
/// Before this fix the field was silently dropped at the request
/// deserializer; overflow replies then arrived with
/// `response_to_client_message_id: null` and the speculative-queue
/// BRAVO bubble never rendered (its reply clobbered ALPHA's bubble
/// via the session_result merge path).
#[tokio::test]
async fn chat_request_accepts_legacy_client_message_id_alias_to_inbound() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(ApiState {
            inbound_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let body = serde_json::json!({
        "message": "Use shell: echo BRAVO",
        "session_id": "web-fa12f",
        "client_message_id": "client-bravo-xyz",
        "stream": true,
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let inbound = tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
        .await
        .expect("handle_chat must forward the message to the gateway bus")
        .expect("inbound channel closed without a message");

    assert_eq!(
        inbound.message_id.as_deref(),
        Some("client-bravo-xyz"),
        "InboundMessage.message_id must carry the request's client_message_id \
             so the overflow reply can be routed back to the correct bubble",
    );
}

/// Empty / missing `thread_id` (including via the legacy
/// `client_message_id` alias) must be rejected before inbound dispatch.
#[tokio::test]
async fn chat_request_treats_empty_client_message_id_as_absent() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(ApiState {
            inbound_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let body = serde_json::json!({
        "message": "hello",
        "session_id": "web-fa12f-empty",
        "client_message_id": "",
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let no_inbound =
        tokio::time::timeout(std::time::Duration::from_millis(100), inbound_rx.recv()).await;
    assert!(
        matches!(no_inbound, Err(_) | Ok(None)),
        "empty thread_id/client_message_id must be rejected before inbound dispatch",
    );
}

/// The `/chat` handler must construct a TurnContext before it emits the
/// synthetic warm-up event. The event leaves the server through the same
/// envelope sink as later stream events.
#[tokio::test]
async fn chat_request_builds_turn_context_before_first_event() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(ApiState {
            inbound_tx,
            pending: pending.clone(),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let body = serde_json::json!({
        "message": "hello",
        "session_id": "web-636-warmup",
        "thread_id": "cmid-warmup-key",
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let inbound = tokio::time::timeout(std::time::Duration::from_millis(500), inbound_rx.recv())
        .await
        .expect("inbound channel timed out")
        .expect("inbound channel closed without a message");
    assert_eq!(inbound.message_id.as_deref(), Some("cmid-warmup-key"));
    assert_eq!(
        inbound
            .metadata
            .get("thread_id")
            .and_then(|value| value.as_str()),
        Some("cmid-warmup-key")
    );

    let sink = test_ui_sink();
    let ctx = test_turn_context("web-636-warmup", "cmid-warmup-key");
    let warmup = initial_sse_events(&sink, &ctx, false).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&warmup[0]).unwrap();
    assert_eq!(parsed["type"], "thinking");
    assert_eq!(parsed["event_type"], "thinking");
    assert_eq!(parsed["session_id"], "web-636-warmup");
    assert_eq!(parsed["thread_id"], "cmid-warmup-key");
    assert_eq!(parsed["event_seq"], 1);
    assert_eq!(parsed["payload"]["thread_id"], "cmid-warmup-key");
}

/// Missing `thread_id` is rejected at ingress before any event can be
/// emitted or inbound work can be queued.
#[tokio::test]
async fn chat_request_without_thread_id_is_rejected() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(ApiState {
            inbound_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let body = serde_json::json!({
        "message": "no cmid",
        "session_id": "web-636-no-cmid",
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let no_inbound =
        tokio::time::timeout(std::time::Duration::from_millis(100), inbound_rx.recv()).await;
    assert!(
        matches!(no_inbound, Err(_) | Ok(None)),
        "missing thread_id must be rejected before inbound dispatch",
    );
}

#[tokio::test]
async fn attach_only_does_not_enqueue_empty_inbound_message() {
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(ApiState {
            inbound_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"","session_id":"web-attach","thread_id":"cmid-attach","media":[],"attach_only":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    if let Ok(Some(message)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), inbound_rx.recv()).await
    {
        panic!(
            "attach_only unexpectedly enqueued an inbound turn: {:?}",
            message.content
        );
    }
}

#[tokio::test]
async fn session_status_reports_background_tasks_separately_from_stream_activity() {
    let app = Router::new()
        .route("/sessions/{id}/status", get(handle_session_status))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: Some(Arc::new(|_| {
                serde_json::json!([
                    { "id": "task-1", "tool_name": "run_pipeline", "status": "running" }
                ])
            })),
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions/web-attach/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload.get("active").and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        payload
            .get("has_bg_tasks")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        payload
            .get("has_deferred_files")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
}

#[tokio::test]
async fn session_status_accepts_profiled_api_session_ids() {
    let app = Router::new()
        .route("/sessions/{id}/status", get(handle_session_status))
        .route("/sessions/{id}/tasks", get(handle_session_tasks))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            sessions: test_sessions(),
            task_query: Some(Arc::new(|session_key| {
                if session_key == "dspfac:api:web-profiled" {
                    serde_json::json!([
                        { "id": "task-1", "tool_name": "Deep research", "status": "running" }
                    ])
                } else {
                    serde_json::json!([])
                }
            })),
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sessions/dspfac:api:web-profiled/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let body = axum::body::to_bytes(status.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload
            .get("has_bg_tasks")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let tasks = app
        .oneshot(
            Request::builder()
                .uri("/sessions/dspfac:api:web-profiled/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tasks.status(), StatusCode::OK);
    let body = axum::body::to_bytes(tasks.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.as_array().map(Vec::len), Some(1));
    assert_eq!(payload[0]["tool_name"], "Deep research");
}

#[test]
fn message_info_from_history_message_hides_absolute_paths() {
    let data_dir = tempfile::tempdir().unwrap();
    let artifact = data_dir
        .path()
        .join("users")
        .join("dspfac%3Aapi%3Aweb-1")
        .join("workspace")
        .join(".artifacts")
        .join("deck.pptx");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"pptx").unwrap();

    let message = Message {
        role: MessageRole::Assistant,
        content: format!("[file:{}] deck.pptx", artifact.to_string_lossy()),
        media: vec![artifact.to_string_lossy().to_string()],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    };

    let info = message_info_from_history_message(&message, data_dir.path(), 7);
    assert_eq!(info.seq, Some(7));
    assert_eq!(info.media.len(), 1);
    assert_ne!(info.media[0], artifact.to_string_lossy());
    assert!(
        !info
            .content
            .contains(&artifact.to_string_lossy().to_string())
    );
    assert!(info.content.contains("[file:pf/"));
}

#[test]
fn message_info_propagates_client_message_id_from_message() {
    let data_dir = tempfile::tempdir().unwrap();
    let message = Message::user("hello there").with_client_message_id("cmid-history-7");

    let info = message_info_from_history_message(&message, data_dir.path(), 5);
    assert_eq!(info.seq, Some(5));
    assert_eq!(info.client_message_id.as_deref(), Some("cmid-history-7"));

    // Round-trip via JSON (the wire shape) — the field is preserved.
    let json = serde_json::to_value(&info).unwrap();
    assert_eq!(json["client_message_id"], "cmid-history-7");
}

#[test]
fn message_info_omits_client_message_id_when_absent() {
    let data_dir = tempfile::tempdir().unwrap();
    let message = Message::user("hi");

    let info = message_info_from_history_message(&message, data_dir.path(), 0);
    assert!(info.client_message_id.is_none());

    // Skipped from the serialized JSON for forward compat.
    let json = serde_json::to_value(&info).unwrap();
    assert!(json.get("client_message_id").is_none());
}

#[test]
fn build_session_result_event_normalizes_persisted_media_paths_like_history_replay() {
    let data_dir = tempfile::tempdir().unwrap();
    let artifact = data_dir
        .path()
        .join("users")
        .join("dspfac%3Aapi%3Aweb-1")
        .join("workspace")
        .join(".artifacts")
        .join("deck.pptx");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"pptx").unwrap();

    let raw = serde_json::json!({
        "role": "assistant",
        "content": "Deck ready",
        "media": [artifact.to_string_lossy().to_string()],
        "timestamp": Utc::now().to_rfc3339(),
    });

    let event = build_session_result_event(&raw, data_dir.path(), None, Some("slides demo"))
        .expect("session result event");
    let event_media = event["message"]["media"]
        .as_array()
        .expect("event media array")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    let replay_media = message_info_from_history_message(
        &Message {
            role: MessageRole::Assistant,
            content: "Deck ready".into(),
            media: vec![artifact.to_string_lossy().to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        },
        data_dir.path(),
        1,
    )
    .media;

    assert_eq!(event_media, replay_media);
}

#[tokio::test]
async fn api_channel_persists_media_without_legacy_file_marker_content() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let channel = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let artifact = data_dir.path().join("deck.pptx");
    std::fs::write(&artifact, b"pptx").unwrap();

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "web-legacy-media".into(),
        content: "".into(),
        reply_to: None,
        media: vec![artifact.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "thread_id": "legacy-media-thread" }),
    };

    channel.send(&msg).await.unwrap();

    let info = {
        let sess = sessions.lock().await;
        let data_dir = sess.data_dir();
        let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "web-legacy-media");
        let loaded = sess.load(&key).await.unwrap();
        message_info_from_history_message(&loaded.messages[0], &data_dir, 0)
    };

    assert_eq!(info.media.len(), 1);
    assert!(info.content.trim().is_empty());
}

#[test]
fn copy_media_into_session_artifacts_reuses_existing_copy_for_identical_file() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join(".artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let source = root
        .path()
        .join("slides")
        .join("demo")
        .join("output")
        .join("deck.pptx");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"same deck bytes").unwrap();

    let first = ApiChannel::copy_media_into_session_artifacts(
        &artifact_dir,
        &[source.display().to_string()],
    );
    let second = ApiChannel::copy_media_into_session_artifacts(
        &artifact_dir,
        &[source.display().to_string()],
    );

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0], second[0]);
    assert!(std::path::Path::new(&first[0]).exists());
}

#[test]
fn api_session_key_candidates_prefer_current_profile() {
    let keys = api_session_key_candidates(Some("dspfac--newsbot"), "web-123", None);

    assert_eq!(keys[0].0, "dspfac--newsbot:api:web-123");
    assert_eq!(keys[1].0, "_main:api:web-123");
    assert_eq!(keys[2].0, "api:web-123");
}

#[test]
fn api_session_key_candidates_do_not_double_prefix_profiled_ids() {
    let keys = api_session_key_candidates(Some("dspfac"), "dspfac:api:web-123", None);
    let rendered = keys.iter().map(|key| key.0.as_str()).collect::<Vec<_>>();

    assert_eq!(rendered[0], "dspfac:api:web-123");
    assert!(rendered.contains(&"api:web-123"));
    assert!(!rendered.contains(&"dspfac:api:dspfac:api:web-123"));
}

#[test]
fn api_chat_id_from_profiled_session_key_strips_prefix() {
    assert_eq!(
        api_chat_id_from_session_key("dspfac--newsbot:api:web-123"),
        Some("web-123")
    );
    assert_eq!(
        api_chat_id_from_session_key("_main:api:web-123"),
        Some("web-123")
    );
    assert_eq!(api_chat_id_from_session_key("api:web-123"), Some("web-123"));
}

#[test]
fn api_chat_id_from_session_key_hides_internal_runtime_topics() {
    assert_eq!(
        api_chat_id_from_session_key("dspfac:api:web-123#child-task-1"),
        None
    );
    assert_eq!(
        api_chat_id_from_session_key("dspfac:api:web-123#default.tasks"),
        None
    );
    assert_eq!(api_chat_id_from_session_key("web-123#default.tasks"), None);
    assert_eq!(
        api_chat_id_from_session_key("dspfac:api:web-123#research"),
        Some("web-123#research")
    );
    assert_eq!(
        api_chat_id_from_session_key("web-123#research"),
        Some("web-123#research")
    );
    assert_eq!(api_chat_id_from_session_key("telegram:123"), None);
}

#[test]
fn api_channel_name() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    assert_eq!(ch.name(), "api");
}

#[test]
fn api_channel_max_message_length() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    assert_eq!(ch.max_message_length(), 1_000_000);
}

#[test]
fn initial_sse_events_include_thinking_envelope() {
    let sink = test_ui_sink();
    let ctx = test_turn_context("test-chat", "cmid-warmup-A");
    let events = initial_sse_events(&sink, &ctx, false).unwrap();
    assert_eq!(events.len(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(parsed["type"], "thinking");
    assert_eq!(parsed["event_type"], "thinking");
    assert_eq!(parsed["iteration"], 0);
    assert_eq!(parsed["session_id"], "test-chat");
    assert_eq!(parsed["thread_id"], "cmid-warmup-A");
    assert_eq!(parsed["event_seq"], 1);
    assert_eq!(parsed["payload"]["thread_id"], "cmid-warmup-A");
}

#[test]
fn initial_sse_events_include_preprocessing_for_media() {
    let sink = test_ui_sink();
    let ctx = test_turn_context("test-chat", "cmid-warmup-B");
    let events = initial_sse_events(&sink, &ctx, true).unwrap();
    assert_eq!(events.len(), 2);
    let parsed: Vec<serde_json::Value> = events
        .iter()
        .map(|event| serde_json::from_str(event).unwrap())
        .collect();
    assert_eq!(parsed[0]["type"], "thinking");
    assert_eq!(parsed[0]["thread_id"], "cmid-warmup-B");
    assert_eq!(parsed[0]["event_seq"], 1);
    assert_eq!(parsed[1]["type"], "tool_progress");
    assert_eq!(parsed[1]["tool"], "preprocessing");
    assert_eq!(parsed[1]["thread_id"], "cmid-warmup-B");
    assert_eq!(parsed[1]["event_seq"], 2);
}

#[test]
fn ui_event_sink_overwrites_stale_payload_thread_id() {
    let sink = test_ui_sink();
    let ctx = test_turn_context("test-chat", "cmid-canonical");
    let raw = serde_json::json!({
        "type": "thinking",
        "iteration": 0,
        "thread_id": "stale",
    });
    let encoded = sink.encode(&ctx, raw).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(parsed["thread_id"], "cmid-canonical");
    assert_eq!(parsed["payload"]["thread_id"], "cmid-canonical");
}

#[tokio::test]
async fn sse_channel_bounds_buffer_and_drops_oldest_events() {
    let (tx, mut rx) = new_sse_channel();
    for i in 0..=SSE_CHANNEL_CAPACITY {
        let _ = tx.send(i.to_string());
    }

    assert!(matches!(
        rx.recv().await,
        Err(broadcast::error::RecvError::Lagged(1))
    ));
    assert_eq!(rx.recv().await.unwrap(), "1");
}

#[test]
fn build_bg_task_tool_start_events_adds_tts_compatibility_event() {
    let tasks = serde_json::json!([
        { "id": "task-1", "tool_name": "Direct TTS", "tool_call_id": "call_tts_1", "status": "running" },
        { "id": "task-2", "tool_name": "Direct TTS", "status": "spawned" },
        { "id": "task-3", "tool_name": "Research Podcast", "status": "running" }
    ]);

    let events = build_bg_task_tool_start_events(&tasks);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "tool_start");
    assert_eq!(events[0]["tool"], "fm_tts");
    assert_eq!(events[0]["tool_call_id"], "call_tts_1");
}

#[tokio::test]
async fn send_to_pending_client() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat".into(),
        content: "hello world".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({ "thread_id": "test-thread" }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "replace");
    assert_eq!(parsed["text"], "hello world");
}

#[tokio::test]
async fn send_committed_background_result_emits_session_result_event() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat".into(), tx);
    }

    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("podcast.mp3");
    std::fs::write(&source, b"audio").unwrap();

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat".into(),
        content: "Status: SUCCESS".into(),
        reply_to: None,
        media: vec![source.to_string_lossy().to_string()],
        metadata: serde_json::json!({
            "_history_persisted": true,
            "thread_id": "bg-result-thread",
            "_session_result": {
                "seq": 7,
                "role": "assistant",
                "content": "Status: SUCCESS",
                "timestamp": "2026-04-15T19:15:03Z",
                "media": [source.to_string_lossy().to_string()],
                "thread_id": "bg-result-thread",
            }
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "session_result");
    assert_eq!(parsed["message"]["seq"], 7);
    assert_eq!(parsed["message"]["content"], "Status: SUCCESS");
    let media = parsed["message"]["media"].as_array().unwrap();
    assert_eq!(media.len(), 1);
    assert!(media[0].as_str().unwrap().starts_with("pf/"));
    assert!(rx.try_recv().is_err());
}

/// Regression for rapid concurrent turns: when a long-running background
/// task originating in turn A finally finalises after later turns have
/// started, the wire event MUST carry turn A's explicit thread_id.
///
/// Reproduces the live mini3 trace (2026-04-29, session
/// `web-1777402538752-zn7jfr`) where the deep_research turn's late
/// output landed under the voices turn's bubble — the bug this PR
/// fixes.
#[tokio::test]
async fn late_tool_result_for_overflow_turn_keeps_originating_thread_id_under_3_user_race() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-race-chat".into(), tx);
    }

    // Now turn A's background task finally finalises. It carries
    // `thread_id=cmid-A-deep-research` in OutboundMessage metadata.
    // The api_channel must stamp the event from that explicit owner.
    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-race-chat".into(),
        content: "Deep research report on space exploration".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_history_persisted": true,
            "thread_id": "cmid-A-deep-research",
            "_session_result": {
                "seq": 9,
                "role": "assistant",
                "content": "Deep research report on space exploration",
                "timestamp": "2026-04-29T05:56:03Z",
                "media": [],
                "thread_id": "cmid-A-deep-research",
            }
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "session_result");
    assert_eq!(
        parsed.get("thread_id").and_then(|v| v.as_str()),
        Some("cmid-A-deep-research"),
        "wire-side session_result MUST be tagged with the turn A cmid \
             carried explicitly in OutboundMessage metadata. Got: {parsed}"
    );
    assert_eq!(
        parsed["message"].get("thread_id").and_then(|v| v.as_str()),
        Some("cmid-A-deep-research"),
        "the embedded message body must also carry the originating \
             thread_id so the web client renders it under the right bubble \
             (the v2 thread-store keys off `message.thread_id`)"
    );
}

/// Explicit `thread_id` in OutboundMessage metadata stamps the
/// `replace`/wire-side text path.
#[tokio::test]
async fn explicit_metadata_thread_id_stamps_replace_event() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-explicit-chat".into(), tx);
    }

    // Outbound carries A explicitly.
    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-explicit-chat".into(),
        content: "originating turn A reply".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({ "thread_id": "cmid-explicit-A" }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "replace");
    assert_eq!(
        parsed.get("thread_id").and_then(|v| v.as_str()),
        Some("cmid-explicit-A"),
        "explicit metadata.thread_id must stamp the event; got {parsed}"
    );
}

#[tokio::test]
async fn broadcasts_session_result_for_user_message_with_client_message_id() {
    // Verifies that the api_channel `send()` path emits a session_result
    // event for a persisted *user* message when the OutboundMessage carries
    // `_session_result` metadata with role="user" and a client_message_id.
    // This is the wire shape the web client uses to stamp the
    // server-assigned `historySeq` onto its optimistic user bubble (the
    // M8.10-A-counterpart fix for user messages).
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-user-msg-chat".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-user-msg-chat".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_history_persisted": true,
            "thread_id": "cmid-user-bubble-42",
            "_session_result": {
                "seq": 4,
                "role": "user",
                "content": "remind me about lunch",
                "timestamp": "2026-04-24T19:15:03Z",
                "client_message_id": "cmid-user-bubble-42",
                "thread_id": "cmid-user-bubble-42",
            }
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "session_result");
    assert_eq!(parsed["message"]["role"], "user");
    assert_eq!(parsed["message"]["seq"], 4);
    assert_eq!(parsed["message"]["content"], "remind me about lunch");
    assert_eq!(
        parsed["message"]["client_message_id"], "cmid-user-bubble-42",
        "user-message session_result events must carry the client-supplied id so the web client can correlate optimistic bubbles to the server seq"
    );
    assert!(rx.try_recv().is_err());
}

/// Helper: simulate the actor-side write to per-user JSONL (no FLAT write).
/// Mirrors `SessionActor::deliver_background_notification` for spawn_only
/// file deliveries — the actor stamps `_history_persisted=true` on the
/// outbound, so ApiChannel never writes for these.
///
/// PR F (M8.10): pre-stamp `thread_id` on Assistant/Tool rows before
/// the canonical persist's new-write fail-closed split runs. Mirrors
/// the production `fallback_thread_id_for_assistant` helper.
async fn actor_persist_to_per_user(
    data_dir: &Path,
    session_key: &SessionKey,
    mut message: Message,
) {
    let mut handle = crate::session::SessionHandle::open(data_dir, session_key);
    if message.thread_id.is_none()
        && matches!(
            message.role,
            octos_core::MessageRole::Assistant | octos_core::MessageRole::Tool
        )
    {
        message.thread_id = handle
            .session()
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, octos_core::MessageRole::User))
            .and_then(|user| {
                user.thread_id
                    .clone()
                    .or_else(|| user.client_message_id.clone())
            })
            .or_else(|| Some(uuid::Uuid::now_v7().to_string()));
    }
    handle.add_message_with_seq(message).await.unwrap();
}

#[tokio::test]
async fn bus_side_persist_refuses_unbound_assistant_rows() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let channel = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    let result = channel
        .persist_to_session("web-unbound", None, Message::assistant("unowned assistant"))
        .await;

    assert!(
        result.is_none(),
        "unbound assistant rows must not be persisted through the API migration path"
    );
    let encoded_base =
        crate::session::encode_path_component(&format!("{TEST_PROFILE_ID}:api:web-unbound"));
    let canonical = data_dir
        .path()
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join("default.jsonl");
    assert!(
        !canonical.exists(),
        "fail-closed unbound persist must not create {}",
        canonical.display()
    );

    let mut sess = sessions.lock().await;
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "web-unbound");
    assert!(sess.get_or_create(&key).await.get_history(10).is_empty());
}

#[tokio::test]
async fn bus_side_persist_routes_to_canonical_per_user_topic_jsonl() {
    // Pins the unified-write contract introduced by the storage unification
    // fix. The bus-side `persist_to_session` previously wrote to:
    //   - legacy flat `sessions/<encoded_full_key>.jsonl`
    //   - hardcoded per-user `users/<encoded_base>/sessions/default.jsonl`
    //     (ignored topic — actor-side writes used `<topic>.jsonl`)
    //
    // Post-fix it must route through `SessionHandle` so writes land in the
    // canonical per-user `<encoded_topic>.jsonl` file the actor uses.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let channel = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    );

    let topic = "site astro";
    let mp3 = data_dir.path().join("audio").join("a.mp3");
    std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
    std::fs::write(&mp3, b"mp3 bytes").unwrap();

    let outbound = OutboundMessage {
        channel: "api".into(),
        chat_id: "web-canonical".into(),
        content: "✓ fm_tts done".into(),
        reply_to: None,
        media: vec![mp3.to_string_lossy().into_owned()],
        metadata: serde_json::json!({ "topic": topic, "thread_id": "canonical-thread" }),
    };
    channel.send(&outbound).await.unwrap();

    // Canonical per-user topic file must exist with the message.
    let encoded_base =
        crate::session::encode_path_component(&format!("{TEST_PROFILE_ID}:api:web-canonical"));
    let encoded_topic = crate::session::encode_path_component(topic);
    let canonical = data_dir
        .path()
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.jsonl"));
    assert!(
        canonical.exists(),
        "bus-side persist must write to canonical per-user `<topic>.jsonl` ({}) — \
             this is the file the SessionActor also writes, eliminating split-brain storage",
        canonical.display()
    );
    let body = std::fs::read_to_string(&canonical).unwrap();
    assert!(
        body.contains("fm_tts done"),
        "canonical per-user `<topic>.jsonl` must record the persisted message"
    );

    // Legacy per-user `default.jsonl` mirror must NOT be written when a
    // topic is supplied — that's the bug we are fixing.
    let legacy_default = data_dir
        .path()
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join("default.jsonl");
    assert!(
        !legacy_default.exists(),
        "topic-bearing bus-side persist must NOT touch the hardcoded `default.jsonl` mirror — \
             that legacy fan-out caused the split-brain bug"
    );
}

#[tokio::test]
async fn spawn_only_file_delivery_is_visible_to_watcher_replay_after_reconnect() {
    // Regression for the split-brain session-storage bug.
    //
    // Production scenario reproduced on mini2 (2026-04-23):
    //   1. A spawn_only background task (e.g. fm_tts) finishes long after
    //      the user's interactive turn ended, so the live SSE pending
    //      sender for the session has either been dropped or is empty.
    //   2. SessionActor::deliver_background_notification persists the file
    //      message via the per-actor `SessionHandle` (per-user JSONL at
    //      `users/<encoded_base>/sessions/<encoded_topic>.jsonl`) and stamps
    //      `_history_persisted=true` on the OutboundMessage.
    //   3. ApiChannel::send sees `_history_persisted=true` and skips its
    //      own bus-side write (legacy flat layout
    //      `sessions/<encoded_full_key>.jsonl` plus the hardcoded per-user
    //      `default.jsonl` mirror — note that mirror IGNORES the actor's
    //      topic).
    //   4. The user reconnects. Their web client opens
    //      `/sessions/{chat_id}/events/stream` — without re-supplying the
    //      topic in the query string (a real failure mode of the dashboard
    //      reload + workflow listing flows). The only chance the audio
    //      bubble has to materialise is `replay_committed_session_results`.
    //
    // Pre-fix, the actor's write lands in `<topic>.jsonl` while the
    // ApiChannel write — when it happens at all — hits FLAT or the
    // hardcoded `default.jsonl`. With no topic in the candidate-key set
    // and nothing in the topic-less per-user files, replay returns zero
    // events and the audio bubble silently disappears.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let topic = "site astro";
    let session_key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "test-chat", Some(topic));

    // Actor persists the user message into per-user `<topic>.jsonl` (the
    // production gateway path: SessionActor handles inbound BEFORE
    // ApiChannel.send is ever invoked for this turn).
    actor_persist_to_per_user(
        data_dir.path(),
        &session_key,
        Message::user("please make me a podcast about cats"),
    )
    .await;

    // Simulate the mp3 the spawn_only fm_tts skill produced.
    let mp3_path = data_dir.path().join("artifacts").join("podcast.mp3");
    std::fs::create_dir_all(mp3_path.parent().unwrap()).unwrap();
    std::fs::write(&mp3_path, b"ID3...mp3 bytes").unwrap();

    // Actor-side spawn_only delivery — the only writer that records the
    // file message anywhere. ApiChannel never writes because the actor
    // stamps `_history_persisted=true`.
    actor_persist_to_per_user(
        data_dir.path(),
        &session_key,
        Message {
            role: MessageRole::Assistant,
            content: "✓ fm_tts completed — file delivered".into(),
            media: vec![mp3_path.to_string_lossy().into_owned()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    )
    .await;

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    // Cold reconnect WITHOUT topic — this is what fails pre-fix because
    // the candidate-key set never reaches `<topic>.jsonl` and the
    // hardcoded per-user `default.jsonl` mirror was never written.
    let replayed_topicless =
        replay_committed_session_results(&state, "test-chat", None, None).await;

    let event_topicless = replayed_topicless
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .find(|payload| {
            payload["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("fm_tts completed"))
        });
    assert!(
        event_topicless.is_some(),
        "topic-less reconnect must still surface the spawn_only file delivery — \
             actor-side write to per-user `<topic>.jsonl` was lost because the \
             bus-side reader never visits topic-bearing per-user files when the \
             watcher subscribes without a topic. This is the split-brain \
             session-storage bug"
    );

    // Hot reconnect WITH topic — this happens to work pre-fix because the
    // SessionManager::load merge sees per-user `<topic>.jsonl`. We pin
    // the contract here too so the canonical-write fix doesn't quietly
    // break the topic path while landing the topic-less path.
    let replayed_topic =
        replay_committed_session_results(&state, "test-chat", None, Some(topic)).await;
    let event_topic = replayed_topic
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .find(|payload| {
            payload["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("fm_tts completed"))
        })
        .expect("topic-aware reconnect must continue to surface the file delivery");
    let media = event_topic["message"]["media"]
        .as_array()
        .expect("file-delivery session_result event must carry a media array");
    assert_eq!(media.len(), 1, "exactly one audio handle expected");
    assert!(
        media[0].as_str().unwrap_or_default().starts_with("pf/"),
        "audio path must be projected through the profile-relative file handle so the web client can fetch it"
    );
}

#[tokio::test]
async fn concurrent_bus_side_persists_get_distinct_seqs() {
    // Regression for the concurrent-persist seq race introduced when
    // `persist_to_session` switched from `SessionManager::add_message_with_seq`
    // (shared mutex via `Arc<Mutex<SessionManager>>`) to
    // `SessionHandle::open` + `add_message_with_seq`.
    //
    // Each `SessionHandle::open` loads disk into its OWN per-instance
    // `messages: Vec<_>`. Two concurrent calls both observe `len = N`,
    // both append, both return `seq = N`. Watcher correlation breaks —
    // the web client sees two "session_result, seq=N" rows and renders
    // duplicates.
    //
    // Post-fix: writes for the same session_key must serialise at the
    // storage layer (per-key mutex map shared across actor + channel) so
    // each call observes a fresh `len` and returns a distinct seq.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let channel = Arc::new(ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    ));

    let chat_id = "race-chat";
    let topic = Some("race-topic");
    let n = 16usize;

    let mut handles = Vec::new();
    for i in 0..n {
        let channel = channel.clone();
        let chat_id = chat_id.to_string();
        handles.push(tokio::spawn(async move {
            channel
                .persist_to_session(
                    &chat_id,
                    topic,
                    Message::assistant_with_thread(
                        format!("concurrent assistant {i}"),
                        ThreadId::new(format!("race-thread-{i}")),
                    ),
                )
                .await
                .and_then(|info| info.seq)
        }));
    }

    let mut seqs: Vec<usize> = Vec::with_capacity(n);
    for h in handles {
        let result = h.await.expect("join");
        seqs.push(result.expect("persist must succeed and return a seq"));
    }
    seqs.sort();
    let expected: Vec<usize> = (0..n).collect();
    assert_eq!(
        seqs, expected,
        "{n} concurrent bus-side persist calls must each receive a \
             distinct sequence in 0..N (storage layer must serialise writes \
             via a per-key lock map shared across actor + channel)"
    );
}

#[tokio::test]
async fn topic_less_fallback_runs_when_candidate_topicless_file_is_empty() {
    // Regression for the topic-less-fallback short-circuit bug.
    //
    // When a topic-less candidate JSONL exists on disk but contains zero
    // displayable assistant messages (only user lines, or only tool-trace
    // assistant entries with empty content), the candidate-load early-
    // returned with `events = []` BEFORE the topic-less per-user fallback
    // ran. As a result the audio bubble committed under a topic-bearing
    // file was never surfaced to a topic-less reconnect.
    //
    // Post-fix: the fallback path runs whenever the candidate-load returned
    // empty content (vs returned a Some(session) with displayable rows). A
    // populated topic-bearing per-user JSONL must surface even when an
    // empty topic-less per-user file co-exists for the same base_key.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());

    // Topic-less per-user JSONL: exists, but only contains a user line —
    // zero displayable assistant content.
    let topicless_key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "fallback-chat", None);
    actor_persist_to_per_user(
        data_dir.path(),
        &topicless_key,
        Message::user("hello — no assistant response yet on this branch"),
    )
    .await;

    // Topic-bearing per-user JSONL: holds the actually-committed audio
    // bubble that the topic-less reconnect must replay.
    let topic = "site astro";
    let topic_key = current_profile_api_session_key_with_topic(
        Some(TEST_PROFILE_ID),
        "fallback-chat",
        Some(topic),
    );
    let mp3 = data_dir.path().join("audio").join("fallback.mp3");
    std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
    std::fs::write(&mp3, b"mp3 bytes").unwrap();
    actor_persist_to_per_user(
        data_dir.path(),
        &topic_key,
        Message {
            role: MessageRole::Assistant,
            content: "✓ topic-bearing audio bubble committed under topic JSONL".into(),
            media: vec![mp3.to_string_lossy().into_owned()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    )
    .await;

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed = replay_committed_session_results(&state, "fallback-chat", None, None).await;
    let topic_event = replayed
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .find(|payload| {
            payload["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("topic-bearing audio bubble"))
        });
    assert!(
        topic_event.is_some(),
        "topic-less reconnect must reach the per-user fallback when the \
             candidate topic-less JSONL is empty — the early `return events;` \
             in the candidate-load loop short-circuited the fallback and the \
             topic-bearing audio bubble silently disappeared"
    );
}

#[tokio::test]
async fn topic_less_fallback_does_not_strip_messages_via_per_file_seq() {
    // Regression for the wrong-axis `since_seq` filter in the topic-less
    // fallback. Pre-fix, `since_seq` was compared against per-file
    // `enumerate()` positions inside EACH topic JSONL independently — a
    // watcher cursor of N meant "skip N messages of every topic file"
    // instead of "skip the first N messages in the unified replay".
    // For any topic file with > N messages this either wrongly stripped
    // legitimate later assistant rows or wrongly let early rows through.
    //
    // Post-fix, the fallback path drops the per-file `since_seq` filter
    // entirely. The fallback only runs on a topic-less reconnect — that
    // is, the watcher has no unified cursor against which a per-file
    // index could be measured. Tracking it was meaningless. We pin the
    // contract: with `since_seq=Some(N)` the fallback still emits every
    // displayable assistant message regardless of position in its file.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());

    // Topic-less per-user JSONL is empty so the candidate-load returns
    // no events; the fallback is the only path that surfaces the audio
    // bubbles below.
    let topicless_key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "long-topic-chat", None);
    actor_persist_to_per_user(
        data_dir.path(),
        &topicless_key,
        Message::user("kick off the topic"),
    )
    .await;

    // Topic-bearing per-user JSONL with many displayable assistant rows.
    // Pre-fix, with `since_seq=Some(5)` the fallback would silently strip
    // rows 0..=5 from the topic file's per-file index (so messages 0-5
    // would be dropped).
    let topic = "long-topic";
    let topic_key = current_profile_api_session_key_with_topic(
        Some(TEST_PROFILE_ID),
        "long-topic-chat",
        Some(topic),
    );
    for n in 0..20usize {
        actor_persist_to_per_user(
            data_dir.path(),
            &topic_key,
            Message::assistant(format!("topic answer {n}")),
        )
        .await;
    }

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed = replay_committed_session_results(&state, "long-topic-chat", Some(5), None).await;
    let recovered: std::collections::HashSet<String> = replayed
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .filter_map(|payload| payload["message"]["content"].as_str().map(str::to_string))
        .collect();
    for n in 0..20usize {
        let expected = format!("topic answer {n}");
        assert!(
            recovered.contains(&expected),
            "fallback must surface every displayable assistant message in \
                 a topic file regardless of `since_seq` (a per-watcher cursor \
                 measured against the unified replay, NOT the per-file index). \
                 Missing: `{expected}`"
        );
    }
}

#[tokio::test]
async fn combined_replay_events_are_globally_sorted_by_timestamp() {
    // Pins the global-timestamp-sort contract for the combined-events
    // branch in `replay_committed_session_results`. Pre-fix, when both
    // the candidate-load and the topic-less fallback produced events,
    // the function concatenated `candidate_events` (in disk order) BEFORE
    // `fallback_events` (timestamp-sorted) without globally sorting the
    // unified set. If the two branches' timestamps interleave, replay
    // delivered them out of chronological order — the web client renders
    // bubbles in delivery order, so a topic-less reconnect would show
    // candidate bubbles first then a "leap back in time" to fallback
    // bubbles whose timestamps fall between candidate ones.
    //
    // Post-fix: extract the timestamp from each event's payload and
    // sort the unified set by timestamp before returning.
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());

    // Topic-less candidate JSONL with timestamps T0, T2, T4 (even).
    let topicless_key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "interleave-chat", None);
    let base = chrono::Utc::now() - chrono::Duration::seconds(60);
    for (idx, secs) in [0i64, 2, 4].iter().enumerate() {
        let mut msg = Message::assistant(format!("candidate-{idx}-T{secs}"));
        msg.timestamp = base + chrono::Duration::seconds(*secs);
        actor_persist_to_per_user(data_dir.path(), &topicless_key, msg).await;
    }

    // Topic-bearing fallback file under the same base_key with timestamps
    // T1, T3, T5 (odd) — interleaving the candidate timestamps.
    let topic = "interleaved";
    let topic_key = current_profile_api_session_key_with_topic(
        Some(TEST_PROFILE_ID),
        "interleave-chat",
        Some(topic),
    );
    for (idx, secs) in [1i64, 3, 5].iter().enumerate() {
        let mut msg = Message::assistant(format!("fallback-{idx}-T{secs}"));
        msg.timestamp = base + chrono::Duration::seconds(*secs);
        actor_persist_to_per_user(data_dir.path(), &topic_key, msg).await;
    }

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    // Topic-less reconnect — both the candidate-load and the topic-less
    // fallback produce events.
    let replayed = replay_committed_session_results(&state, "interleave-chat", None, None).await;

    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = replayed
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .filter_map(|payload| {
            payload["message"]["timestamp"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
        })
        .collect();

    assert_eq!(
        timestamps.len(),
        6,
        "combined replay must surface all six events: {replayed:?}"
    );

    let mut sorted = timestamps.clone();
    sorted.sort();
    assert_eq!(
        timestamps, sorted,
        "combined replay must be globally sorted by timestamp; got {timestamps:?}"
    );

    // Spot-check the chronological interleave (candidate-T0, fallback-T1, ...).
    let contents: Vec<String> = replayed
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        .filter_map(|payload| payload["message"]["content"].as_str().map(str::to_string))
        .collect();
    let expected_order = [
        "candidate-0-T0",
        "fallback-0-T1",
        "candidate-1-T2",
        "fallback-1-T3",
        "candidate-2-T4",
        "fallback-2-T5",
    ];
    assert_eq!(
        contents, expected_order,
        "combined replay must interleave by timestamp"
    );
}

#[tokio::test]
async fn replay_committed_session_results_replays_only_newer_assistant_messages() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key = current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "test-chat", None);

    {
        let mut manager = sessions.lock().await;
        manager
            .add_message_with_seq(&key, Message::user("hello"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "first result",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "✓ report completed — file delivered",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
    }

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed = replay_committed_session_results(&state, "test-chat", Some(1), None).await;

    assert_eq!(replayed.len(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
    assert_eq!(parsed["type"], "session_result");
    assert_eq!(parsed["message"]["seq"], 2);
    assert_eq!(parsed["message"]["role"], "assistant");
    assert_eq!(
        parsed["message"]["content"],
        "✓ report completed — file delivered"
    );
}

#[tokio::test]
async fn replay_committed_session_results_without_since_seq_replays_all_assistant_messages() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key = current_profile_api_session_key_with_topic(
        Some(TEST_PROFILE_ID),
        "test-chat",
        Some("slides launch"),
    );
    let deck_path = data_dir.path().join("slides").join("final-deck.pptx");
    std::fs::create_dir_all(deck_path.parent().unwrap()).unwrap();
    std::fs::write(&deck_path, b"deck").unwrap();

    {
        let mut manager = sessions.lock().await;
        manager
            .add_message_with_seq(&key, Message::user("hello"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "first result",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message {
                    role: MessageRole::Assistant,
                    content: "final deck".to_string(),
                    media: vec![deck_path.to_string_lossy().into_owned()],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    // PR F: pre-stamp for the new-write fail-closed split.
                    thread_id: Some("test-thread".to_string()),
                    timestamp: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
    }

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed =
        replay_committed_session_results(&state, "test-chat", None, Some("slides launch")).await;

    assert_eq!(replayed.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(&replayed[1]).unwrap();
    assert_eq!(first["type"], "session_result");
    assert_eq!(first["topic"], "slides launch");
    assert_eq!(first["message"]["seq"], 1);
    assert_eq!(first["message"]["content"], "first result");
    assert_eq!(second["type"], "session_result");
    assert_eq!(second["topic"], "slides launch");
    assert_eq!(second["message"]["seq"], 2);
    let media = second["message"]["media"].as_array().unwrap();
    assert_eq!(media.len(), 1);
    assert!(media[0].as_str().unwrap().starts_with("pf/"));
}

#[test]
fn should_drop_replayed_session_result_only_for_already_replayed_seq() {
    let replayed = serde_json::json!({
        "type": "session_result",
        "message": {
            "seq": 7,
            "role": "assistant",
            "content": "done",
        }
    })
    .to_string();
    let newer = serde_json::json!({
        "type": "session_result",
        "message": {
            "seq": 8,
            "role": "assistant",
            "content": "later",
        }
    })
    .to_string();
    let replace = serde_json::json!({
        "type": "replace",
        "text": "partial",
    })
    .to_string();

    assert!(should_drop_replayed_session_result(&replayed, Some(7)));
    assert!(should_drop_replayed_session_result(&replayed, Some(9)));
    assert!(!should_drop_replayed_session_result(&newer, Some(7)));
    assert!(!should_drop_replayed_session_result(&replace, Some(7)));
    assert!(!should_drop_replayed_session_result(&replayed, None));
}

#[test]
fn session_result_seq_from_payload_reads_message_seq() {
    let payload = serde_json::json!({
        "type": "session_result",
        "message": {
            "seq": 3,
            "role": "assistant",
            "content": "hello",
        }
    })
    .to_string();
    let no_seq = serde_json::json!({
        "type": "session_result",
        "message": {
            "role": "assistant",
            "content": "hello",
        }
    })
    .to_string();

    assert_eq!(session_result_seq_from_payload(&payload), Some(3));
    assert_eq!(session_result_seq_from_payload(&no_seq), None);
    assert_eq!(session_result_seq_from_payload("{not-json"), None);
}

#[tokio::test]
async fn replay_committed_session_results_skips_empty_assistant_tool_trace_messages() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "tool-heavy-chat", None);

    {
        let mut manager = sessions.lock().await;
        manager
            .add_message_with_seq(&key, Message::user("查一下他的背景 John Ternus"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "search",
                    serde_json::json!({"query": "John Ternus 背景"}),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "get_time",
                    serde_json::json!({
                        "timezone": "America/Los_Angeles",
                        "current_date": "2026-04-20"
                    }),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "activate_tools",
                    serde_json::json!({"tools": ["cron"]}),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "John Ternus is Apple's SVP of hardware engineering.",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(&key, Message::user("你有哪些定时任务"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(&key, Message::user("提醒我 10 分钟后喝水，我在 PDT 时区"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "get_time",
                    serde_json::json!({"timezone": "America/Los_Angeles"}),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "activate_tools",
                    serde_json::json!({"tools": ["cron"]}),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message(
                    "cron",
                    serde_json::json!({"action": "add", "in_minutes": 10}),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(&key, Message::user("记住我的时区"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "好的，已记住你的时区为 PDT（America/Los_Angeles）。",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
        // Trailing empty tool-trace assistant message previously could overwrite
        // the visible final answer in client reconciliation.
        manager
            .add_message_with_seq(
                &key,
                assistant_tool_call_message("cron", serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();
    }

    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions,
        task_query: None,
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed = replay_committed_session_results(&state, "tool-heavy-chat", None, None).await;

    assert_eq!(replayed.len(), 2);
    for event in &replayed {
        let parsed: serde_json::Value = serde_json::from_str(event).unwrap();
        let content = parsed["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        assert!(!content.is_empty());
    }
    let last: serde_json::Value = serde_json::from_str(replayed.last().unwrap()).unwrap();
    assert_eq!(
        last["message"]["content"],
        "好的，已记住你的时区为 PDT（America/Los_Angeles）。"
    );
}

#[tokio::test]
async fn replay_task_status_events_replays_current_tasks_with_topic() {
    let state = ApiState {
        inbound_tx: mpsc::channel(1).0,
        pending: Arc::new(Mutex::new(HashMap::new())),
        watchers: Arc::new(Mutex::new(HashMap::new())),
        auth_token: None,
        profile_id: Some(TEST_PROFILE_ID.to_string()),
        sessions: test_sessions(),
        task_query: Some(Arc::new(|_| {
            serde_json::json!([
                {
                    "id": "task-1",
                    "tool_name": "podcast_generate",
                    "status": "running",
                    "started_at": "2026-04-16T00:00:00Z",
                    "runtime_detail": {
                        "schema": "octos.harness.event.v1",
                        "schema_version": 1,
                        "kind": "progress",
                        "session_id": "api:test-chat",
                        "task_id": "task-1",
                        "workflow_kind": "deep_research",
                        "current_phase": "fetching_sources",
                        "progress_message": "Fetching source 3/12",
                        "progress": 0.42
                    },
                    "workflow_kind": "deep_research",
                    "current_phase": "fetching_sources",
                    "error": null
                }
            ])
        })),
        task_cancel: None,
        task_relaunch: None,
        on_session_deleted: None,
        metrics_renderer: None,
        event_seq: Arc::new(StdMutex::new(HashMap::new())),
    };

    let replayed = replay_task_status_events(&state, "test-chat", Some("site astro")).await;

    assert_eq!(replayed.len(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&replayed[0]).unwrap();
    assert_eq!(parsed["type"], "task_status");
    assert_eq!(parsed["topic"], "site astro");
    assert_eq!(parsed["task"]["id"], "task-1");
    assert_eq!(parsed["task"]["tool_name"], "podcast_generate");
    assert_eq!(parsed["task"]["workflow_kind"], "deep_research");
    assert_eq!(parsed["task"]["current_phase"], "fetching_sources");
    assert_eq!(
        parsed["task"]["runtime_detail"]["progress_message"],
        "Fetching source 3/12"
    );
    assert_eq!(parsed["task"]["runtime_detail"]["progress"], 0.42);
}

#[tokio::test]
async fn send_completion_closes_stream() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
            "thread_id": "done-thread",
            "model": "moonshot/kimi-k2.5 @ autodl.art",
            "provider": "moonshot",
            "model_id": "kimi-k2.5",
            "endpoint": "autodl.art",
            "tokens_in": 123,
            "tokens_out": 456,
            "session_cost": 0.0228,
        }),
    };
    ch.send(&msg).await.unwrap();

    // Should receive done event
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "done");
    assert_eq!(parsed["model"], "moonshot/kimi-k2.5 @ autodl.art");
    assert_eq!(parsed["provider"], "moonshot");
    assert_eq!(parsed["model_id"], "kimi-k2.5");
    assert_eq!(parsed["endpoint"], "autodl.art");
    assert_eq!(parsed["tokens_in"], 123);
    assert_eq!(parsed["tokens_out"], 456);
    assert_eq!(parsed["session_cost"], 0.0228);

    // Sender was removed — next recv returns None
    assert!(matches!(
        rx.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
}

#[tokio::test]
async fn should_close_incomplete_completion_with_error_not_success_done() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    ch.pending.lock().await.insert("partial-chat".into(), tx);
    ch.send(&OutboundMessage {
        channel: "api".into(),
        chat_id: "partial-chat".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"_completion": true, "thread_id": "partial-turn",
            "outcome": "incomplete", "truncated": true, "error_code": "max_tokens",
            "error": "Model output was truncated; the response is incomplete",
            "tokens_in": 101, "tokens_out": 23, "cache_read_tokens": 17,
            "cache_write_tokens": 19, "reasoning_tokens": 7, "committed_seq": 9}),
    })
    .await
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(
        event["type"], "error",
        "a truncated turn is not a successful done"
    );
    assert_eq!(event["outcome"], "incomplete");
    assert_eq!(event["truncated"], true);
    assert_eq!(event["code"], "max_tokens");
    assert!(event["content"].as_str().unwrap().contains("incomplete"));
    assert_eq!(event["tokens_in"], 101);
    assert_eq!(event["tokens_out"], 23);
    assert_eq!(event["cache_read_tokens"], 17);
    assert_eq!(event["cache_write_tokens"], 19);
    assert_eq!(event["reasoning_tokens"], 7);
    assert_eq!(event["committed_seq"], 9);
    assert!(matches!(
        rx.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
}

#[tokio::test]
async fn should_deliver_incomplete_overflow_once_without_closing_primary_stream() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut primary) = new_sse_channel();
    ch.pending.lock().await.insert("overflow-chat".into(), tx);
    let (watch_tx, mut watcher) = new_sse_channel();
    ch.watchers
        .lock()
        .await
        .insert(watcher_key("overflow-chat", None), watch_tx);
    ch.send(&OutboundMessage {
        channel: "api".into(),
        chat_id: "overflow-chat".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"thread_id": "overflow-turn", "_history_persisted": true,
            "outcome": "incomplete", "truncated": true,
            "_session_result": {"seq": 9, "role": "assistant", "content": "actual partial",
                "outcome": "incomplete", "truncated": true, "error_code": "max_tokens",
                "tokens_in": 101, "tokens_out": 23}}),
    })
    .await
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&watcher.recv().await.unwrap()).unwrap();
    assert_eq!(event["type"], "session_result");
    assert_eq!(event["message"]["content"], "actual partial");
    assert_eq!(event["message"]["outcome"], "incomplete");
    assert_eq!(event["message"]["tokens_in"], 101);
    assert!(
        watcher.try_recv().is_err(),
        "no duplicate final/error fanout"
    );
    assert!(ch.pending.lock().await.contains_key("overflow-chat"));
    // broadcast_session_event sends the same one result to active SSE too.
    let primary_event: serde_json::Value =
        serde_json::from_str(&primary.recv().await.unwrap()).unwrap();
    assert_eq!(primary_event["type"], "session_result");
    assert!(matches!(
        primary.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn done_event_carries_committed_seq_when_message_persisted() {
    // M8.10-A regression: the SSE `done` event must thread the committed
    // session sequence back to the web client so live-streamed bubbles can
    // populate `historySeq` and avoid floating to the end of the list.
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat-seq".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat-seq".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
            "thread_id": "seq-thread",
            "committed_seq": 42,
            "tokens_in": 10,
            "tokens_out": 5,
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "done");
    assert_eq!(parsed["committed_seq"], 42);
}

/// M8.10 PR #2: every SSE event the API channel emits MUST include
/// `thread_id` (sourced from `OutboundMessage.metadata.thread_id`) so
/// web clients with multiple in-flight threads on the same chat_id
/// can route streamed events to the right per-thread bubble.
#[tokio::test]
async fn done_event_includes_thread_id_from_metadata() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat-tid".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat-tid".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
            "committed_seq": 11,
            "thread_id": "cmid-thread-Z",
            "tokens_in": 0,
            "tokens_out": 0,
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "done");
    assert_eq!(parsed["committed_seq"], 11);
    assert_eq!(parsed["thread_id"], "cmid-thread-Z");
}

/// M8.10 PR #2: the wire-side `replace` event emitted by `send`
/// (non-streaming assistant content) must carry thread_id.
#[tokio::test]
async fn replace_event_includes_thread_id_from_metadata() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat-replace".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat-replace".into(),
        content: "hello world".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "thread_id": "cmid-thread-R",
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "replace");
    assert_eq!(parsed["text"], "hello world");
    assert_eq!(parsed["thread_id"], "cmid-thread-R");
}

/// M8.10 PR #2: streaming `token` and `replace` events emitted via
/// `edit_message` must carry thread_id encoded into the synthetic
/// message_id returned by `send_with_id`. This is the key handshake
/// that lets two concurrent threads on the same chat_id be
/// demultiplexed by web clients.
#[tokio::test]
async fn edit_message_token_event_includes_thread_id_decoded_from_message_id() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-edit".into(), tx);
    }

    // Step 1: send_with_id encodes thread_id into the message_id
    let initial = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-edit".into(),
        content: "Hi".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "thread_id": "cmid-thread-EDIT",
        }),
    };
    let message_id = ch.send_with_id(&initial).await.unwrap().unwrap();
    // Drain the initial replace event from send().
    let _ = rx.recv().await.unwrap();

    // Step 2: edit_message decodes thread_id back from message_id and
    // tags the streaming `token`/`replace` payload with it.
    ch.edit_message("chat-edit", &message_id, "Hi there")
        .await
        .unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    // The delta comparison sees prev="Hi" and new starts with "Hi", so
    // it emits a `token` for the suffix.
    assert_eq!(parsed["type"], "token");
    assert_eq!(parsed["text"], " there");
    assert_eq!(parsed["thread_id"], "cmid-thread-EDIT");
}

/// A streaming send without an explicit thread_id must fail closed. The
/// previous implementation recovered from a per-chat sticky map; M9
/// requires the stream forwarder to pass the TurnContext-owned id.
#[tokio::test]
async fn send_with_id_without_thread_id_errors_before_live_emit() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-sticky".into(), tx);
    }

    let stream_initial = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-sticky".into(),
        content: "Hi".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"streaming": true}),
    };
    let err = ch
        .send_with_id(&stream_initial)
        .await
        .expect_err("send_with_id must reject missing thread_id");
    assert!(err.to_string().contains("required thread_id"));
    assert!(rx.try_recv().is_err(), "no unowned replace may be emitted");
}

/// Raw SSE emission must fail closed when neither the bound argument nor
/// the payload carries a thread_id. The old sticky fallback is removed.
#[tokio::test]
async fn send_raw_sse_without_thread_id_errors() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-thinking".into(), tx);
    }

    let raw_thinking = serde_json::json!({"type": "thinking", "iteration": 0});
    let err = ch
        .send_raw_sse("chat-thinking", &raw_thinking.to_string())
        .await
        .expect_err("missing thread_id must fail closed");
    assert!(err.to_string().contains("required thread_id"));
    assert!(rx.try_recv().is_err(), "no unowned event may reach SSE");
}

/// When raw SSE JSON already carries a thread_id, `send_raw_sse` emits
/// through the envelope sink. A later untagged raw event fails instead of
/// inheriting that earlier id.
#[tokio::test]
async fn send_raw_sse_preserves_explicit_thread_id_and_rejects_untagged() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-explicit".into(), tx);
    }

    let tagged = serde_json::json!({
        "type": "thinking",
        "iteration": 0,
        "thread_id": "cmid-explicit-A",
    });
    ch.send_raw_sse("chat-explicit", &tagged.to_string())
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["thread_id"], "cmid-explicit-A");
    assert_eq!(parsed["event_type"], "thinking");

    let untagged = serde_json::json!({"type": "thinking", "iteration": 1});
    let err = ch
        .send_raw_sse("chat-explicit", &untagged.to_string())
        .await
        .expect_err("untagged raw SSE must not inherit prior event thread_id");
    assert!(err.to_string().contains("required thread_id"));
    assert!(rx.try_recv().is_err(), "no inherited sticky event expected");
}

/// #649 follow-up (rapid-fire): when 5 chat streams interleave on the
/// same chat_id, each turn's stream forwarder must call `send_with_id`
/// with its OWN cmid in metadata so subsequent `edit_message` /
/// `send_raw_sse` calls can recover the right thread. This asserts each
/// encoded message_id carries its own explicit owner under interleaving.
#[tokio::test]
async fn send_with_id_uses_explicit_metadata_thread_id_under_rapid_fire() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, _rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-rapid-fire".into(), tx);
    }

    // Q1's stream forwarder calls `send_with_id` with metadata that
    // explicitly carries Q1's cmid. The encoded message_id must capture
    // cmid-A, regardless of other in-flight turns on the same chat.
    let q1 = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-rapid-fire".into(),
        content: "first chunk for Q1".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-A",
        }),
    };
    let q1_msg_id = ch
        .send_with_id(&q1)
        .await
        .unwrap()
        .expect("send_with_id always returns Some");
    let (_, q1_decoded) = decode_sse_message_id(&q1_msg_id);
    assert_eq!(
        q1_decoded,
        Some("cmid-A"),
        "Q1's encoded message_id must capture its OWN cmid. Got: {q1_msg_id}"
    );

    // Q3 lands next, also with explicit metadata. Same expectation:
    // Q3's encoded id must reflect Q3's cmid.
    let q3 = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-rapid-fire".into(),
        content: "first chunk for Q3".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-C",
        }),
    };
    let q3_msg_id = ch
        .send_with_id(&q3)
        .await
        .unwrap()
        .expect("send_with_id always returns Some");
    let (_, q3_decoded) = decode_sse_message_id(&q3_msg_id);
    assert_eq!(
        q3_decoded,
        Some("cmid-C"),
        "Q3's encoded message_id must capture its OWN cmid under concurrency. Got: {q3_msg_id}"
    );
}

/// #649 follow-up (rapid-fire): drive an end-to-end interleaved
/// 5-turn rapid-fire scenario through `send` and assert each turn's
/// `replace` event carries its OWN cmid when the OutboundMessage
/// metadata supplies one explicitly. This exercises the path the
/// stream forwarder takes for non-first chunks (the inner `send` call
/// from `send_with_id`).
#[tokio::test]
async fn rapid_fire_streaming_chunks_carry_per_turn_thread_id() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-rapid".into(), tx);
    }

    // Each turn's first chunk arrives via `send` with explicit
    // thread_id metadata. The `replace` event emitted on the wire
    // must be tagged with that cmid.
    let cases = [
        ("cmid-A", "Q1 reply"),
        ("cmid-B", "Q2 reply"),
        ("cmid-C", "Q3 reply"),
        ("cmid-D", "Q4 reply"),
    ];
    for (cmid, content) in &cases {
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "chat-rapid".into(),
            content: (*content).into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "streaming": true,
                "thread_id": cmid,
            }),
        };
        ch.send(&msg).await.unwrap();
        // Reset last_content so each turn's chunk emits as a `replace`,
        // not a delta `token` (the production stream forwarder calls
        // `send_with_id` first which clears last_content; we mimic that
        // by clearing it inline here).
        ch.last_content.lock().await.remove("chat-rapid");
    }

    // Drain wire events and verify each carries its OWN cmid.
    let mut events: Vec<serde_json::Value> = Vec::new();
    while let Ok(payload) = rx.try_recv() {
        events.push(serde_json::from_str(&payload).unwrap());
    }
    let replaces: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["type"] == "replace").collect();
    assert_eq!(
        replaces.len(),
        cases.len(),
        "expected {} replace events, got {}: {:?}",
        cases.len(),
        replaces.len(),
        events,
    );
    for ((expected_cmid, expected_text), event) in cases.iter().zip(replaces.iter()) {
        assert_eq!(
            event["text"], *expected_text,
            "replace event text mismatch: {event}"
        );
        assert_eq!(
            event["thread_id"], *expected_cmid,
            "replace event for {expected_text} mis-tagged. Expected {expected_cmid}, got: {event}"
        );
    }
}

/// overflow-stress regression (#680 follow-up): when two concurrent
/// streams on the same chat have prefix-overlapping content, the
/// `chat_id`-only `last_content` key let turn A's prev poison turn B's
/// delta computation. A specific failure mode observed in the live
/// soak: turn A produces "Hello" first, turn B then sends its own
/// independent "Hello world" as a fresh `replace` chunk — pre-fix,
/// `edit_message` saw `prev["chat"]="Hello"` and emitted a misleading
/// `token` delta " world" tagged with thread B, with the result that
/// the web client painted A's earlier content under B's user bubble.
/// Per-(chat, thread) keying isolates the two streams so each computes
/// its delta against its OWN prev.
///
/// The post-fix wire shape: turn A's edit emits a `token` delta from
/// its own prev; turn B's first edit emits a full `replace` (because
/// no prev exists for B yet). Either way, neither stream cross-talks.
#[tokio::test]
async fn concurrent_same_chat_streams_do_not_cross_talk_via_last_content() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-overflow".into(), tx);
    }

    // Step 1: Turn A starts streaming. send_with_id seeds prev for A.
    let a_initial = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-overflow".into(),
        content: "Hello".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-A",
        }),
    };
    let a_msg_id = ch.send_with_id(&a_initial).await.unwrap().unwrap();
    // Drain the initial replace event for A.
    let _ = rx.recv().await.unwrap();

    // Step 2: Turn B starts streaming on the SAME chat with a DIFFERENT
    // thread_id. send_with_id must NOT inherit A's prev as B's seed —
    // that's the cross-talk root cause.
    let b_initial = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-overflow".into(),
        content: "Hello world".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-B",
        }),
    };
    let b_msg_id = ch.send_with_id(&b_initial).await.unwrap().unwrap();
    // Drain the initial replace event for B.
    let _ = rx.recv().await.unwrap();

    // Step 3: Turn A's stream forwarder emits the next chunk via
    // edit_message. Pre-fix, the chat-only key now holds B's "Hello
    // world" so A's edit computed `prev = "Hello world"` (not a prefix
    // of A's "Hello there") → emitted a wasteful full `replace`. With
    // per-thread keying, A's prev is its OWN "Hello", so the delta
    // " there" emits as a `token` tagged for cmid-A.
    ch.edit_message("chat-overflow", &a_msg_id, "Hello there")
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "cmid-A",
        "edit on A must tag thread_id=cmid-A, not B's. Got: {parsed}"
    );
    assert_eq!(
        parsed["type"], "token",
        "A's edit should emit a token delta against A's own prev. Got: {parsed}"
    );
    assert_eq!(
        parsed["text"], " there",
        "A's delta must be from A's own prev (\"Hello\"), not B's (\"Hello world\"). Got: {parsed}"
    );

    // Step 4: Turn B's edit_message arrives next with content that
    // happens to share A's "Hello there" prefix. Pre-fix, A's just-
    // recorded "Hello there" would seed prev["chat"], and B's "Hello
    // there is something" would emit a `token` " is something" stamped
    // with cmid-B that contained text *originally produced by A*. With
    // per-thread keying, B's prev is "Hello world" (B's own seed), and
    // "Hello there is something" does NOT start with "Hello world", so
    // we fall through to the safe `replace` path.
    ch.edit_message("chat-overflow", &b_msg_id, "Hello there is something")
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "cmid-B",
        "B's edit must tag thread_id=cmid-B. Got: {parsed}"
    );
    // The critical regression assertion: B's wire payload must NEVER
    // contain a token delta that *starts with* content originally
    // produced by A. We assert the non-token shape (full replace)
    // since "Hello there is something" doesn't extend B's own prev
    // ("Hello world").
    assert_eq!(
        parsed["type"], "replace",
        "B should emit a full replace (B's prev was \"Hello world\", not a prefix of \"Hello there is something\"). Pre-fix this leaked a cmid-B-tagged token delta containing A's content. Got: {parsed}"
    );
    assert_eq!(parsed["text"], "Hello there is something");
}

/// overflow-stress regression: when one concurrent stream finalizes
/// (`done`), the `last_content` cleanup must drop ONLY that turn's
/// per-thread entry — never wipe a sibling turn's prev. Without per-
/// thread keying, A's `done` cleared the chat-wide key, forcing the
/// next B chunk to emit a wasteful `replace`. Worse, since the
/// chat-only key had been seeded by whichever turn last ran, A's
/// `done` could discard B's prev entirely. Per-thread keying scopes
/// the cleanup to A and leaves B's stream state untouched.
#[tokio::test]
async fn done_cleanup_does_not_wipe_concurrent_thread_last_content() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-cleanup".into(), tx);
    }

    // Turn A and turn B both seed last_content under their own
    // per-thread keys.
    let a = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-cleanup".into(),
        content: "Apples".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-A",
        }),
    };
    let _ = ch.send_with_id(&a).await.unwrap().unwrap();
    let _ = rx.recv().await.unwrap();
    let b = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-cleanup".into(),
        content: "Bananas".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "streaming": true,
            "thread_id": "cmid-B",
        }),
    };
    let b_msg_id = ch.send_with_id(&b).await.unwrap().unwrap();
    let _ = rx.recv().await.unwrap();

    // Turn A finalizes first. The `done` cleanup must scope its
    // last_content removal to cmid-A only — not blow away B's seed.
    let a_done = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-cleanup".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
            "thread_id": "cmid-A",
        }),
    };
    ch.send(&a_done).await.unwrap();
    // Re-add the broadcast subscriber, since `_completion` removes
    // the pending channel.
    let (tx2, mut rx2) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-cleanup".into(), tx2);
    }

    // Turn B's next chunk should still see B's prev ("Bananas") and
    // emit a delta `token` for the suffix. Pre-fix this would have
    // emitted a wasteful full `replace` (or worse, nothing visible)
    // because A's done wiped the chat-only prev key.
    ch.edit_message("chat-cleanup", &b_msg_id, "Bananas are yellow")
        .await
        .unwrap();
    let event = rx2.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["thread_id"], "cmid-B");
    assert_eq!(
        parsed["type"], "token",
        "B's prev ('Bananas') must survive A's done cleanup. Got: {parsed}"
    );
    assert_eq!(parsed["text"], " are yellow");
}

/// M9: live completion events without explicit thread ownership must fail
/// closed rather than emitting an ambiguous unowned `done` event.
#[tokio::test]
async fn done_event_without_thread_id_errors_before_live_emit() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat-no-tid".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat-no-tid".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
        }),
    };
    let err = ch
        .send(&msg)
        .await
        .expect_err("completion without thread_id must fail closed");

    assert!(err.to_string().contains("required thread_id"));
    assert!(
        rx.try_recv().is_err(),
        "no unowned done event may be emitted"
    );
}

#[tokio::test]
async fn done_event_omits_committed_seq_when_persist_failed_or_skipped() {
    // M8.10-A: when the server has no committed seq (e.g. persist failed
    // or is skipped), the done event must NOT include `committed_seq` so
    // legacy/error-path behaviour is preserved.
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-chat-noseq".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat-noseq".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_completion": true,
            "thread_id": "noseq-thread",
            "tokens_in": 10,
            "tokens_out": 5,
        }),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "done");
    assert!(
        parsed.get("committed_seq").is_none() || parsed["committed_seq"].is_null(),
        "committed_seq must be omitted when missing from metadata, got: {parsed}"
    );
}

#[tokio::test]
async fn send_completion_with_bg_tasks_closes_and_client_polls() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("test.mp3");
    std::fs::write(&source, b"bg-audio").unwrap();
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-bg".into(), tx);
    }

    // Send completion with has_bg_tasks = true
    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-bg".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"_completion": true, "has_bg_tasks": true, "thread_id": "bg-thread"}),
    };
    ch.send(&msg).await.unwrap();

    // Should receive done event with has_bg_tasks flag
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "done");
    assert_eq!(parsed["has_bg_tasks"], true);

    // SSE closes immediately — client will poll session history
    assert!(matches!(
        rx.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));

    // Background file arrives later — persisted to session history
    let file_msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-bg".into(),
        content: String::new(),
        reply_to: None,
        media: vec![source.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "thread_id": "bg-thread" }),
    };
    ch.send(&file_msg).await.unwrap();

    // Client polling session history would find it
    let mut sess = sessions.lock().await;
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg");
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(10);
    let stored = history
        .iter()
        .flat_map(|m| m.media.iter())
        .find(|path| path.ends_with("test.mp3"))
        .cloned()
        .expect("expected persisted artifact path");
    assert_ne!(stored, source.to_string_lossy().to_string());
    assert!(Path::new(&stored).exists());
}

#[tokio::test]
async fn send_completion_with_bg_tasks_emits_compat_tool_start_before_done() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    )
    .with_task_query(Arc::new(|_| {
        serde_json::json!([
            {
                "id": "task-1",
                "tool_name": "Direct TTS",
                "tool_call_id": "call_tts_1",
                "status": "running"
            }
        ])
    }));
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-bg-compat".into(), tx);
    }

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-bg-compat".into(),
        content: String::new(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"_completion": true, "has_bg_tasks": true, "thread_id": "bg-compat-thread"}),
    };
    ch.send(&msg).await.unwrap();

    let first: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    let second: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();

    assert_eq!(first["type"], "tool_start");
    assert_eq!(first["tool"], "fm_tts");
    assert_eq!(first["tool_call_id"], "call_tts_1");
    assert_eq!(second["type"], "done");
    assert_eq!(second["has_bg_tasks"], true);
}

#[tokio::test]
async fn send_file_message_persists_to_session() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("test.mp3");
    std::fs::write(&source, b"audio").unwrap();

    // Send a file message (no active SSE needed — goes straight to session)
    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-file".into(),
        content: "Audio file".into(),
        reply_to: None,
        media: vec![source.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "thread_id": "file-thread" }),
    };
    ch.send(&msg).await.unwrap();

    // Verify it was persisted to the session
    let mut sess = sessions.lock().await;
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-file");
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(10);
    assert_eq!(history.len(), 1);
    assert!(history[0].content.contains("Audio file"));
    assert_eq!(history[0].media.len(), 1);
    let persisted = &history[0].media[0];
    assert_ne!(persisted, &source.to_string_lossy().to_string());
    assert!(!history[0].content.contains(persisted));
    assert!(Path::new(persisted).exists());
    assert_eq!(std::fs::read(Path::new(persisted)).unwrap(), b"audio");
}

#[tokio::test]
async fn send_file_message_emits_committed_session_result_event() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("test-file".into(), tx);
    }

    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("report.pdf");
    std::fs::write(&source, b"report").unwrap();

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-file".into(),
        content: "Generated report".into(),
        reply_to: None,
        media: vec![source.to_string_lossy().to_string()],
        metadata: serde_json::json!({"tool_call_id": "call_report_1", "thread_id": "report-thread"}),
    };
    ch.send(&msg).await.unwrap();

    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(parsed["type"], "session_result");
    let message = parsed["message"].as_object().expect("message payload");
    assert_eq!(
        message.get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert!(
        message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("Generated report")
    );
    assert_eq!(
        message.get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_report_1")
    );
    let media = message
        .get("media")
        .and_then(|v| v.as_array())
        .expect("media array");
    assert_eq!(media.len(), 1);
    let persisted = media[0].as_str().expect("persisted path");
    assert!(persisted.starts_with("pf/"));
}

#[tokio::test]
async fn send_file_message_keeps_distinct_artifacts_for_same_basename() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let source_a_dir = data_dir.path().join("source-a");
    let source_b_dir = data_dir.path().join("source-b");
    std::fs::create_dir_all(&source_a_dir).unwrap();
    std::fs::create_dir_all(&source_b_dir).unwrap();
    let source_a = source_a_dir.join("report.pdf");
    let source_b = source_b_dir.join("report.pdf");
    std::fs::write(&source_a, b"alpha").unwrap();
    std::fs::write(&source_b, b"beta").unwrap();

    for source in [&source_a, &source_b] {
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "collision-chat".into(),
            content: "report".into(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({ "thread_id": "collision-thread" }),
        };
        ch.send(&msg).await.unwrap();
    }

    let mut sess = sessions.lock().await;
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "collision-chat");
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(10);
    assert_eq!(history.len(), 2);
    let first = history[0].media[0].clone();
    let second = history[1].media[0].clone();
    assert_ne!(first, second);
    assert_eq!(std::fs::read(Path::new(&first)).unwrap(), b"alpha");
    assert_eq!(std::fs::read(Path::new(&second)).unwrap(), b"beta");
}

#[tokio::test]
async fn send_file_message_reuses_existing_session_artifact() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "artifact-chat");
    let artifact_dir = ApiChannel::session_artifact_dir(data_dir.path(), &key);
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let existing = artifact_dir.join("existing.wav");
    std::fs::write(&existing, b"persisted").unwrap();

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "artifact-chat".into(),
        content: "existing".into(),
        reply_to: None,
        media: vec![existing.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "thread_id": "artifact-thread" }),
    };
    ch.send(&msg).await.unwrap();

    let mut sess = sessions.lock().await;
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(10);
    let persisted = std::fs::canonicalize(&history[0].media[0]).unwrap();
    let existing = std::fs::canonicalize(&existing).unwrap();
    assert_eq!(persisted, existing);
}

#[tokio::test]
async fn send_file_message_with_topic_persists_to_topic_session() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("deck.pptx");
    std::fs::write(&source, b"pptx").unwrap();

    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "topic-file-chat".into(),
        content: "".into(),
        reply_to: None,
        media: vec![source.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "topic": "slides demo", "thread_id": "topic-file-thread" }),
    };
    ch.send(&msg).await.unwrap();

    let mut sess = sessions.lock().await;
    let topic_key =
        SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", "topic-file-chat", "slides demo");
    let base_key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "topic-file-chat");
    let topic_history = sess
        .get_or_create(&topic_key)
        .await
        .get_history(10)
        .to_vec();
    let base_history = sess.get_or_create(&base_key).await.get_history(10).to_vec();

    assert_eq!(topic_history.len(), 1);
    assert!(base_history.is_empty());
    assert_eq!(topic_history[0].media.len(), 1);
    assert!(topic_history[0].media[0].contains(".artifacts"));
    assert!(topic_history[0].media[0].contains("deck.pptx"));
}

#[tokio::test]
async fn slides_topic_suppresses_duplicate_deck_delivery_until_new_user_message() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    let topic_key =
        SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", "slides-chat", "slides demo");
    {
        let mut sess = sessions.lock().await;
        sess.add_message(&topic_key, Message::user("go"))
            .await
            .unwrap();
    }

    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let first = source_dir.join("deck-one.pptx");
    let second = source_dir.join("deck-two.pptx");
    std::fs::write(&first, b"pptx-one").unwrap();
    std::fs::write(&second, b"pptx-two").unwrap();

    for source in [&first, &second] {
        let msg = OutboundMessage {
            channel: "api".into(),
            chat_id: "slides-chat".into(),
            content: String::new(),
            reply_to: None,
            media: vec![source.to_string_lossy().to_string()],
            metadata: serde_json::json!({ "topic": "slides demo", "thread_id": "slides-thread-1" }),
        };
        ch.send(&msg).await.unwrap();
    }

    let mut sess = sessions.lock().await;
    let history = sess
        .get_or_create(&topic_key)
        .await
        .get_history(10)
        .to_vec();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[1].role, MessageRole::Assistant);
    assert_eq!(history[1].media.len(), 1);
    assert!(history[1].media[0].contains("deck-one.pptx"));
}

#[tokio::test]
async fn slides_topic_allows_new_deck_after_new_user_message() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    let topic_key =
        SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", "slides-chat-2", "slides demo");
    {
        let mut sess = sessions.lock().await;
        sess.add_message(&topic_key, Message::user("go"))
            .await
            .unwrap();
    }

    let source_dir = data_dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let first = source_dir.join("deck-one.pptx");
    let second = source_dir.join("deck-two.pptx");
    std::fs::write(&first, b"pptx-one").unwrap();
    std::fs::write(&second, b"pptx-two").unwrap();

    let first_msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "slides-chat-2".into(),
        content: String::new(),
        reply_to: None,
        media: vec![first.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "topic": "slides demo", "thread_id": "slides-thread-1" }),
    };
    ch.send(&first_msg).await.unwrap();

    {
        let mut sess = sessions.lock().await;
        sess.add_message(&topic_key, Message::user("regenerate"))
            .await
            .unwrap();
    }

    let second_msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "slides-chat-2".into(),
        content: String::new(),
        reply_to: None,
        media: vec![second.to_string_lossy().to_string()],
        metadata: serde_json::json!({ "topic": "slides demo", "thread_id": "slides-thread-2" }),
    };
    ch.send(&second_msg).await.unwrap();

    let mut sess = sessions.lock().await;
    let history = sess
        .get_or_create(&topic_key)
        .await
        .get_history(10)
        .to_vec();
    let assistant_media = history
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .flat_map(|message| message.media.iter())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(history.len(), 4);
    assert_eq!(assistant_media.len(), 2);
    assert!(assistant_media[0].contains("deck-one.pptx"));
    assert!(assistant_media[1].contains("deck-two.pptx"));
}

#[tokio::test]
async fn send_bg_notification_persists_to_session() {
    let sessions = test_sessions();
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    // Send background task notification (checkmark)
    let notify = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-bg2".into(),
        content: "\u{2713} fm_tts completed \u{2014} file delivered".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({ "thread_id": "bg-notify-thread" }),
    };
    ch.send(&notify).await.unwrap();

    // Verify it was persisted to the session (not sent via SSE)
    let mut sess = sessions.lock().await;
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg2");
    let session = sess.get_or_create(&key).await;
    let history = session.get_history(10);
    assert_eq!(history.len(), 1);
    assert!(history[0].content.contains("fm_tts completed"));
}

#[tokio::test]
async fn send_bg_notification_skips_duplicate_persist_when_history_is_already_written() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key = SessionKey::with_profile(TEST_PROFILE_ID, "api", "test-bg3");
    {
        let mut sess = sessions.lock().await;
        sess.add_message(
            &key,
            Message::assistant_with_thread(
                "✓ fm_tts completed — file delivered",
                octos_core::ThreadId::new("test-thread"),
            ),
        )
        .await
        .unwrap();
    }

    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions.clone(),
        Some(TEST_PROFILE_ID.to_string()),
    );

    let notify = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-bg3".into(),
        content: "\u{2713} fm_tts completed \u{2014} file delivered".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({ "_history_persisted": true }),
    };
    ch.send(&notify).await.unwrap();

    let mut sess = sessions.lock().await;
    let session = sess.get_or_create(&key).await;
    assert_eq!(session.get_history(10).len(), 1);
}

#[tokio::test]
async fn send_to_unknown_chat_is_noop() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let msg = OutboundMessage {
        channel: "api".into(),
        chat_id: "nonexistent".into(),
        content: "hello".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({}),
    };
    // Should not error
    ch.send(&msg).await.unwrap();
}

#[tokio::test]
async fn list_sessions_dedups_profile_scoped_duplicates_by_chat_id() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let mut sess = sessions.lock().await;
    sess.add_message(
        &SessionKey::with_profile("dspfac", "api", "slides-123"),
        Message::user("one"),
    )
    .await
    .unwrap();
    sess.add_message(
        &SessionKey::with_profile(MAIN_PROFILE_ID, "api", "slides-123"),
        Message::user("two"),
    )
    .await
    .unwrap();
    drop(sess);

    let app = Router::new()
        .route("/sessions", get(handle_list_sessions))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions,
            profile_id: Some("dspfac".into()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let matching: Vec<&serde_json::Value> = sessions
        .iter()
        .filter(|entry| entry.get("id").and_then(|id| id.as_str()) == Some("slides-123"))
        .collect();
    assert_eq!(matching.len(), 1);
}

#[tokio::test]
async fn list_sessions_hides_internal_child_and_task_ledger_sessions() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let parent = SessionKey::with_profile("dspfac", "api", "web-123");
    let child = SessionKey::with_profile_topic("dspfac", "api", "web-123", "child-task-1");
    let task_ledger = SessionKey::with_profile_topic("dspfac", "api", "web-123", "default.tasks");
    let raw_parent = SessionKey("web-raw".to_string());
    let raw_task_ledger = SessionKey("web-raw#default.tasks".to_string());
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
        sess.add_message(&raw_parent, Message::user("raw parent"))
            .await
            .unwrap();
        sess.add_message(&raw_task_ledger, Message::user("raw task ledger"))
            .await
            .unwrap();
    }

    let app = Router::new()
        .route("/sessions", get(handle_list_sessions))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions,
            profile_id: Some("dspfac".into()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
        .collect();

    assert_eq!(ids, vec!["web-123", "web-raw"]);
}

#[tokio::test]
async fn session_messages_full_source_reads_from_disk_snapshot() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "web-history", None);

    {
        let mut manager = sessions.lock().await;
        manager
            .add_message_with_seq(&key, Message::user("hello"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "first result",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread(
                    "second result",
                    octos_core::ThreadId::new("test-thread"),
                ),
            )
            .await
            .unwrap();
    }

    let app = Router::new()
        .route("/sessions/{id}/messages", get(handle_session_messages))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions,
            profile_id: Some(TEST_PROFILE_ID.into()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions/web-history/messages?source=full&since_seq=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["seq"], 2);
    assert_eq!(messages[0]["content"], "second result");
}

#[tokio::test]
async fn session_messages_default_source_returns_recent_window_with_absolute_seq() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let key =
        current_profile_api_session_key_with_topic(Some(TEST_PROFILE_ID), "web-history", None);

    {
        let mut manager = sessions.lock().await;
        manager
            .add_message_with_seq(&key, Message::user("one"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread("two", octos_core::ThreadId::new("test-thread-1")),
            )
            .await
            .unwrap();
        manager
            .add_message_with_seq(&key, Message::user("three"))
            .await
            .unwrap();
        manager
            .add_message_with_seq(
                &key,
                Message::assistant_with_thread("four", octos_core::ThreadId::new("test-thread-2")),
            )
            .await
            .unwrap();
    }

    let app = Router::new()
        .route("/sessions/{id}/messages", get(handle_session_messages))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions,
            profile_id: Some(TEST_PROFILE_ID.into()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions/web-history/messages?limit=1&offset=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let messages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["seq"], 3);
    assert_eq!(messages[0]["content"], "four");
}

#[tokio::test]
async fn delete_session_checks_all_profile_candidates() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let id = "web-delete-fallback";
    let main_key = SessionKey::with_profile(MAIN_PROFILE_ID, "api", id);

    {
        let mut sess = sessions.lock().await;
        sess.add_message(&main_key, Message::user("hello"))
            .await
            .unwrap();
        assert!(sess.load(&main_key).await.is_some());
    }

    let app = Router::new()
        .route("/sessions/{id}", delete(handle_delete_session))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions: sessions.clone(),
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let sess = sessions.lock().await;
    assert!(sess.load(&main_key).await.is_none());
}

#[tokio::test]
async fn delete_session_accepts_listed_topic_session_id() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions_in(data_dir.path());
    let id = "web-delete-topic";
    let topic_key = SessionKey::with_profile_topic(TEST_PROFILE_ID, "api", id, "research");

    {
        let mut sess = sessions.lock().await;
        sess.add_message(&topic_key, Message::user("hello"))
            .await
            .unwrap();
        assert!(sess.load(&topic_key).await.is_some());
    }

    let app = Router::new()
        .route("/sessions/{id}", delete(handle_delete_session))
        .with_state(ApiState {
            inbound_tx: mpsc::channel(1).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            auth_token: None,
            sessions: sessions.clone(),
            profile_id: Some(TEST_PROFILE_ID.to_string()),
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Arc::new(StdMutex::new(HashMap::new())),
        });

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/sessions/web-delete-topic%23research")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let fresh = SessionManager::open(data_dir.path()).unwrap();
    assert!(fresh.load(&topic_key).await.is_none());
}

#[tokio::test]
async fn metrics_route_renders_child_prometheus_snapshot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let channel = ApiChannel::new(
        port,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    )
    .with_metrics_renderer(Arc::new(|| {
        "octos_test_metric_total{kind=\"child\"} 7\n".to_string()
    }));

    let (inbound_tx, _inbound_rx) = mpsc::channel(1);
    let shutdown = channel.shutdown.clone();
    let server = tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let body = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("octos_test_metric_total"));
    assert!(body.contains("kind=\"child\""));

    shutdown.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

// ------------------------------------------------------------------
// PR F (M8.10 thread-binding chain `#649 → #740`) — `_bound` variants
// ------------------------------------------------------------------
//
// The `_bound` methods (`edit_message_bound`, `send_raw_sse_bound`)
// pin the M9 invariant: when the originating turn's `thread_id` is in
// the caller's hand, bound ownership wins over stale decoded/payload ids
// and missing ownership fails closed.

/// Bound-turn override invariant: when the caller supplies a `thread_id`
/// via `_bound`, BOTH `edit_message_bound` AND `send_raw_sse_bound` must
/// emit that thread_id even if decoded/payload ids are absent or stale.
/// The invariant: every wire payload is tagged with `thread_id="A"`.
#[tokio::test]
async fn rapid_fire_bound_turn_overrides_stale_ids_for_persist_edit_and_raw_sse() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-rapid-bound".into(), tx);
    }

    // Persist Assistant for turn A using the typed
    // `assistant_with_thread` constructor — codex's PR-A type-system
    // enforcement. The persist must produce a row pinned to A.
    let mut sessions = ch.sessions.lock().await;
    let session_key = octos_core::SessionKey::new("api", "chat-rapid-bound");
    // Seed the session with three siblings so the legacy
    // "derive from most-recent user" walk would find C — the bug
    // shape the bound path beats.
    for cmid in ["A", "B", "C"] {
        let user = octos_core::Message::user_with_cmid(
            format!("question-{cmid}"),
            octos_core::ClientMessageId::new(cmid),
        );
        sessions
            .add_message(&session_key, user)
            .await
            .expect("user persist");
    }
    let assistant_for_a =
        octos_core::Message::assistant_with_thread("answer for A", octos_core::ThreadId::new("A"));
    sessions
        .add_message(&session_key, assistant_for_a)
        .await
        .expect("assistant persist");
    // Reload the in-memory session and assert the assistant row
    // lands under thread A, NOT C (the most-recent user). This is
    // the LEAK 1 closing assertion.
    let session = sessions.get_or_create(&session_key).await;
    let assistant_row = session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, octos_core::MessageRole::Assistant))
        .expect("assistant present");
    assert_eq!(
        assistant_row.thread_id.as_deref(),
        Some("A"),
        "PR F LEAK 1: typed `assistant_with_thread` must persist with the bound \
             thread_id (`A`), NOT derive `C` from most-recent-user history. \
             Got: {:?}",
        assistant_row.thread_id,
    );
    drop(sessions);

    // LEAK 2 wire path: every emitted SSE event for turn A must carry
    // `thread_id=A`. We exercise both
    // the `send_raw_sse_bound` path (used by the stream forwarder for
    // discrete events: thinking, tool_start, ...) and the
    // `edit_message_bound` path (used for streaming `token`/`replace`
    // deltas).

    // Step 1: send_raw_sse_bound with bound=A. Even though the JSON
    // payload has NO thread_id, the bound wins.
    let raw = serde_json::json!({"type": "thinking", "iteration": 0});
    ch.send_raw_sse_bound("chat-rapid-bound", &raw.to_string(), Some("A"))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "A",
        "send_raw_sse_bound must emit bound=A. event: {parsed}"
    );

    // Step 2: send_raw_sse_bound with bound=A AND a stale thread_id
    // already on the payload. Bound MUST overwrite.
    let raw_with_stale = serde_json::json!({
        "type": "thinking",
        "iteration": 1,
        "thread_id": "C-stale"
    });
    ch.send_raw_sse_bound("chat-rapid-bound", &raw_with_stale.to_string(), Some("A"))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "A",
        "send_raw_sse_bound must OVERWRITE stale payload thread_id (C-stale) with bound (A). event: {parsed}"
    );

    // Step 3: send_with_id seeds an encoded message_id (production
    // path captures cmid in the synthetic id). Then edit_message_bound
    // with bound=A must emit thread_id=A even when bound matches the
    // encoded id.
    let initial = OutboundMessage {
        channel: "api".into(),
        chat_id: "chat-rapid-bound".into(),
        content: "Hi".into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({"streaming": true, "thread_id": "A"}),
    };
    let msg_id = ch.send_with_id(&initial).await.unwrap().unwrap();
    // Drain the initial replace event from send_with_id.
    let _ = rx.recv().await.unwrap();

    ch.edit_message_bound("chat-rapid-bound", &msg_id, "Hi there", Some("A"))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "A",
        "edit_message_bound must emit bound=A. event: {parsed}"
    );
}

/// When `edit_message_bound` is called with an explicit `bound` id, it
/// must override the decoded id from the synthetic message id.
#[tokio::test]
async fn edit_message_bound_overrides_decoded_thread_id() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-bound-only".into(), tx);
    }

    // Encoded message_id decodes to X — should be overridden by bound A.
    let encoded = encode_sse_message_id("chat-bound-only", Some("X"));

    // Seed last_content for chat-bound-only/A so the edit emits a
    // `token` delta.
    ch.last_content.lock().await.insert(
        last_content_key("chat-bound-only", Some("A")),
        "Hi".to_string(),
    );

    ch.edit_message_bound("chat-bound-only", &encoded, "Hi there", Some("A"))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    // Bound wins over X (decoded).
    assert_eq!(
        parsed["thread_id"], "A",
        "edit_message_bound must use bound=A, ignoring decoded=X. event: {parsed}"
    );
}

/// PR F: when `send_raw_sse_bound` is called with a bound id and the
/// JSON payload carries a STALE thread_id, bound MUST overwrite.
/// Without the overwrite, a forwarder that pre-rendered its payload
/// before its reporter rebound would leak the stale id on the wire —
/// the exact issue codex flagged at
/// `/tmp/codex-arch-final-review.log:12137+`.
#[tokio::test]
async fn send_raw_sse_bound_overwrites_stale_thread_id_in_payload() {
    let ch = ApiChannel::new(
        8091,
        None,
        Arc::new(AtomicBool::new(false)),
        test_sessions(),
        Some(TEST_PROFILE_ID.to_string()),
    );
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        pending.insert("chat-overwrite".into(), tx);
    }

    let payload = serde_json::json!({
        "type": "thinking",
        "iteration": 0,
        "thread_id": "stale-Z",
    });
    ch.send_raw_sse_bound("chat-overwrite", &payload.to_string(), Some("fresh-A"))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        parsed["thread_id"], "fresh-A",
        "send_raw_sse_bound must overwrite stale payload thread_id with bound id. event: {parsed}"
    );
}
