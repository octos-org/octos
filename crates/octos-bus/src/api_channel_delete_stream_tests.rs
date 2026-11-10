use super::*;

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

async fn chat_body(state: &ApiState, chat: &str, topic: Option<&str>, thread: &str) -> Body {
    let app = Router::new()
        .route("/chat", post(handle_chat))
        .with_state(state.clone());
    let value = serde_json::json!({"message":"waiting", "session_id":chat, "topic":topic, "thread_id":thread});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    response.into_body()
}

async fn delete(state: &ApiState, id: &str) {
    let response = handle_delete_session(
        State(state.clone()),
        HeaderMap::new(),
        axum::extract::Path(id.to_owned()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn topic_requests_have_independent_streams_and_delete_scope() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _inbound) = app_state(test_sessions_in(dir.path()));
    let a = chat_body(&state, "shared", Some("a"), "ta").await;
    let b = chat_body(&state, "shared", Some("b"), "tb").await;
    let other = chat_body(&state, "other", Some("a"), "to").await;
    let a = tokio::spawn(axum::body::to_bytes(a, 10240));
    let b = tokio::spawn(axum::body::to_bytes(b, 10240));
    let other = tokio::spawn(axum::body::to_bytes(other, 10240));
    delete(&state, &format!("{TEST_PROFILE_ID}:api:shared#a")).await;
    tokio::time::timeout(std::time::Duration::from_millis(250), a)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!b.is_finished());
    assert!(!other.is_finished());
    delete(&state, &format!("{TEST_PROFILE_ID}:api:shared")).await;
    tokio::time::timeout(std::time::Duration::from_millis(250), b)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!other.is_finished());
    delete(&state, "other").await;
    tokio::time::timeout(std::time::Duration::from_millis(250), other)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

fn output(chat: &str, thread: &str, completion: bool) -> OutboundMessage {
    OutboundMessage {
        channel: "api".into(),
        chat_id: chat.into(),
        content: if completion {
            String::new()
        } else {
            "late reply".into()
        },
        reply_to: None,
        media: vec![],
        metadata: if completion {
            serde_json::json!({"thread_id":thread,"_completion":true})
        } else {
            serde_json::json!({"thread_id":thread,"streaming":true})
        },
    }
}

fn channel_for(state: &ApiState) -> ApiChannel {
    let mut channel = ApiChannel::new(
        0,
        None,
        Arc::new(AtomicBool::new(false)),
        state.sessions.clone(),
        state.profile_id.clone(),
    );
    channel.pending = state.pending.clone();
    channel.event_seq = state.event_seq.clone();
    channel
}

async fn finish(body: Body) -> String {
    let bytes = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        axum::body::to_bytes(body, 10240),
    )
    .await
    .expect("stream must end promptly")
    .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn deleted_turn_output_cannot_enter_or_close_a_new_thread() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _inbound) = app_state(test_sessions_in(dir.path()));
    let channel = channel_for(&state);
    let old = chat_body(&state, "same", Some("topic"), "old").await;
    delete(&state, "dspfac:api:same#topic").await;
    let old_events = finish(old).await;
    assert!(
        !old_events.contains("\"type\":\"done\""),
        "delete cannot fabricate completion"
    );
    let fresh = chat_body(&state, "same", Some("topic"), "fresh").await;
    channel.send(&output("same", "old", false)).await.unwrap();
    channel.send(&output("same", "old", true)).await.unwrap();
    channel
        .edit_message_bound("same", "ignored", "late edit", Some("old"))
        .await
        .unwrap();
    channel
        .send_raw_sse_bound("same", r#"{"type":"thinking"}"#, Some("old"))
        .await
        .unwrap();
    assert_eq!(state.pending.lock().await.entries.len(), 1);
    channel.send(&output("same", "fresh", true)).await.unwrap();
    let events = finish(fresh).await;
    assert!(events.contains("\"type\":\"done\""));
    assert!(events.contains("\"topic\":\"topic\""));
    assert!(!events.contains("late"));
    assert!(!events.contains("\"thread_id\":\"old\""));
    assert!(state.pending.lock().await.entries.is_empty());
}

#[tokio::test]
async fn delete_failure_keeps_pending_and_the_error_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _inbound) = app_state(test_sessions_in(dir.path()));
    let body = chat_body(&state, "active", None, "a").await;
    // A transcript directory instead of a file fails even when run as root.
    let file = state
        .sessions
        .lock()
        .await
        .data_dir()
        .join("users/dspfac%3Aapi%3Aactive/sessions/default.jsonl");
    std::fs::create_dir_all(&file).unwrap();
    let response = handle_delete_session(
        State(state.clone()),
        HeaderMap::new(),
        axum::extract::Path("dspfac:api:active".into()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(state.pending.lock().await.entries.len(), 1);
    std::fs::remove_dir(&file).unwrap();
    delete(&state, "dspfac:api:active").await;
    finish(body).await;
    assert!(state.pending.lock().await.entries.is_empty());
}

#[tokio::test]
async fn deletion_waits_for_actor_admission_without_blocking_other_chats() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mut inbound) = app_state(test_sessions_in(dir.path()));
    let channel = channel_for(&state);
    let body = chat_body(&state, "chat", None, "turn").await;
    let message = inbound.recv().await.unwrap();
    let key = SessionKey("dspfac:api:chat#foo".into());
    let lease = channel.admit_dispatch(&message, &key).await.unwrap();
    let deleting_state = state.clone();
    let deleting = tokio::spawn(async move { delete(&deleting_state, "chat").await });
    tokio::task::yield_now().await;
    assert!(
        !deleting.is_finished(),
        "deletion raced past actor epoch capture"
    );
    let other = chat_body(&state, "other", None, "other").await;
    channel.send(&output("other", "other", true)).await.unwrap();
    finish(other).await;
    drop(lease);
    deleting.await.unwrap();
    finish(body).await;
}
