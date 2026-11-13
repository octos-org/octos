use super::*;
use axum::body::Body;
use axum::http::{HeaderValue, Request};
use std::path::Path;
use tower::util::ServiceExt;

const TEST_PROFILE_ID: &str = "dspfac";

fn test_sessions_in(data_dir: &Path) -> Arc<Mutex<SessionManager>> {
    Arc::new(Mutex::new(SessionManager::open(data_dir).unwrap()))
}

fn test_sessions() -> Arc<Mutex<SessionManager>> {
    let dir = std::env::temp_dir().join(format!("octos-bus-tests-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    test_sessions_in(&dir)
}

fn test_channel(sessions: Arc<Mutex<SessionManager>>) -> ApiChannel {
    ApiChannel::new(
        0,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        Some(TEST_PROFILE_ID.to_string()),
    )
}

fn app_state(sessions: Arc<Mutex<SessionManager>>) -> (ApiState, mpsc::Receiver<InboundMessage>) {
    let (inbound_tx, inbound_rx) = mpsc::channel(16);
    (
        ApiState {
            inbound_tx,
            pending: Default::default(),
            watchers: Default::default(),
            auth_token: None,
            profile_id: Some(TEST_PROFILE_ID.to_owned()),
            sessions,
            task_query: None,
            task_cancel: None,
            task_relaunch: None,
            on_session_deleted: None,
            metrics_renderer: None,
            event_seq: Default::default(),
        },
        inbound_rx,
    )
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
            source: None,
            participant: None,
        },
    )
    .await;

    let (state, _inbound) = app_state(sessions);

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
    let channel = Arc::new(test_channel(sessions));

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
async fn should_close_incomplete_completion_with_error_not_success_done() {
    let ch = test_channel(test_sessions());
    let (tx, mut rx) = new_sse_channel();
    register_pending(
        &mut *ch.pending.lock().await,
        "partial-chat",
        &["partial-turn"],
        tx,
    );
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
    let ch = test_channel(test_sessions());
    let (tx, mut rx) = new_sse_channel();
    {
        let mut pending = ch.pending.lock().await;
        register_pending(&mut pending, "chat-overflow", &["cmid-A", "cmid-B"], tx);
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
        .with_state(app_state(sessions).0);

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

// ------------------------------------------------------------------
// PR F (M8.10 thread-binding chain `#649 → #740`) — `_bound` variants
// ------------------------------------------------------------------
//
// The `_bound` methods (`edit_message_bound`, `send_raw_sse_bound`)
// pin the M9 invariant: when the originating turn's `thread_id` is in
// the caller's hand, bound ownership wins over stale decoded/payload ids
// and missing ownership fails closed.

// ------------------------------------------------------------------
// #2736 — the chat surface sits behind the same token chain as the
// admin shell, with a constant-time compare. The env/config legs of
// the chain are covered end-to-end in tests/api_channel_token_e2e.rs
// (own process, so the env pivot can't race this binary).
// ------------------------------------------------------------------

#[test]
fn should_extract_bearer_token_from_authorization_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer channel-secret"),
    );
    assert_eq!(
        bearer_token_from_authorization(&headers),
        Some("channel-secret")
    );
    assert!(bearer_token_matches(&headers, "channel-secret"));
    assert!(!bearer_token_matches(&headers, "wrong-secret!!"));
}

#[test]
fn should_return_none_when_authorization_header_missing_or_malformed() {
    let empty = HeaderMap::new();
    assert_eq!(bearer_token_from_authorization(&empty), None);

    let mut basic = HeaderMap::new();
    basic.insert(
        "authorization",
        HeaderValue::from_static("Basic dXNlcjpwYXNz"),
    );
    assert_eq!(bearer_token_from_authorization(&basic), None);

    let mut bare = HeaderMap::new();
    bare.insert("authorization", HeaderValue::from_static("Bearer "));
    assert_eq!(bearer_token_from_authorization(&bare), Some(""));
}

// ------------------------------------------------------------------
// #2751 — the gate covers the WHOLE route table, not just the chat
// surface. The probe walks the real `api_router` — so a route added
// without a probe row stays invisible to this walk: every new route
// must add its handler gate AND a `SURFACE_PROBES` row below, or the
// gap ships open. A missing gate on a probed route fails loudly here.
// ------------------------------------------------------------------

// ------------------------------------------------------------------
// #2328 — the loopback guard in front of the route table: the channel
// binds 127.0.0.1 and speaks to local callers, so a Host naming
// anything else is a rebound/proxied request, and a browser Origin
// outside the loopback is a cross-site page reaching the API directly.
// ------------------------------------------------------------------

#[path = "api_channel_delete_stream_tests.rs"]
mod delete_stream_tests;

fn register_pending(pending: &mut PendingStreams, chat: &str, threads: &[&str], tx: SseSender) {
    for thread in threads {
        pending.insert(
            TurnContext::new(chat, None, ThreadId::new(*thread)),
            tx.clone(),
        );
    }
}
