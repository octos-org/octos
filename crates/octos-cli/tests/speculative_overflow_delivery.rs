//! FA-12 integration test — guards the speculative queue overflow reply
//! wiring end-to-end.
//!
//! FA-11 root-caused two coupled defects. Defect B lives in
//! [`octos_cli::session_actor`]: the speculative overflow task emitted its
//! reply with empty metadata, so [`octos_bus::ApiChannel::send`] routed it
//! only via the `pending[session_id]` channel — which was removed the
//! moment the primary turn emitted its `_completion` marker. The reply was
//! silently dropped and the client's bubble stayed stuck "streaming"
//! forever.
//!
//! The fix threads `_session_result` metadata through the outbound path so
//! [`ApiChannel::send`] can broadcast the committed message onto the
//! watchers fanout (which survives primary-turn completion).
//!
//! The authoritative end-to-end assertion lives as a unit test inside
//! `session_actor.rs` (see
//! `should_emit_session_result_metadata_for_overflow_reply`) because it
//! needs access to the crate-private test fixture helpers
//! (`setup_speculative_actor`, `DelayedMockProvider`, `make_inbound`). This
//! integration test guards the complementary contract on the consumer side:
//! given the exact metadata shape that the overflow path now emits, the
//! `ApiChannel::send` routing contract recognises it as a committed session
//! result and surfaces it on the watchers fanout rather than silently
//! dropping it when `pending` is empty.

#![cfg(feature = "api")]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::api_channel::ApiChannel;
use octos_bus::session::SessionManager;
use octos_bus::Channel;
use octos_core::OutboundMessage;
use tokio::sync::Mutex;

fn test_sessions(dir: &std::path::Path) -> Arc<Mutex<SessionManager>> {
    Arc::new(Mutex::new(
        SessionManager::open(&dir.join("sessions")).unwrap(),
    ))
}

/// Simulates the FA-11 race: primary turn's SSE channel has already been
/// removed from `pending` (after the primary's `_completion` marker), and
/// THEN the overflow reply arrives carrying `_session_result` metadata.
///
/// Before the fix: the overflow's outbound message had empty metadata. It
/// went through the generic `send()` path, found no `pending[chat_id]`, and
/// was silently dropped.
///
/// After the fix: the `_session_result` metadata triggers a committed
/// fanout to the watchers channel — surviving the primary's completion —
/// and the waiting session-event-stream subscriber observes the reply.
#[tokio::test]
async fn should_emit_session_result_metadata_for_overflow_reply() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = test_sessions(data_dir.path());
    let channel = ApiChannel::new(
        8191,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        None,
    );

    // Subscribe a watcher BEFORE the overflow reply arrives — mimics the
    // web client's `GET /api/sessions/:id/events/stream` subscription that
    // is established when the JSON queued-ack comes back from POST /chat.
    let mut watcher_rx = channel
        .subscribe_watcher_for_tests("test-chat", None)
        .await;

    // The overflow reply carries the exact metadata shape that
    // `session_actor::serve_overflow` now emits.
    let overflow_reply = OutboundMessage {
        channel: "api".into(),
        chat_id: "test-chat".into(),
        content: "FA-12 overflow answer payload".into(),
        reply_to: Some("client-msg-bravo".into()),
        media: vec![],
        metadata: serde_json::json!({
            "_history_persisted": true,
            "_session_result": {
                "seq": 42,
                "role": "assistant",
                "content": "FA-12 overflow answer payload",
                "timestamp": "2026-04-23T17:30:00Z",
                "media": [],
                "response_to_client_message_id": "client-msg-bravo",
            }
        }),
    };

    // Act: send the overflow reply. NOTE: we deliberately do NOT populate
    // `pending[test-chat]` — the primary turn's SSE stream has already
    // closed, which is the exact FA-11 race condition.
    channel.send(&overflow_reply).await.unwrap();

    // Assert: the watchers channel sees a `session_result` event carrying
    // the overflow reply. This proves `_session_result` metadata causes
    // `ApiChannel::send` to route through `broadcast_session_event` even
    // when `pending` is empty.
    let event_payload = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        watcher_rx.recv(),
    )
    .await
    .expect("timed out waiting for session_result event on watcher")
    .expect("watcher closed without event");

    let event: serde_json::Value = serde_json::from_str(&event_payload).unwrap();
    assert_eq!(
        event["type"], "session_result",
        "expected session_result event, got: {event}"
    );
    assert_eq!(
        event["message"]["seq"], 42,
        "expected seq=42, got: {}",
        event["message"]
    );
    assert_eq!(
        event["message"]["content"], "FA-12 overflow answer payload",
        "expected overflow content in message, got: {}",
        event["message"]
    );
    assert_eq!(
        event["message"]["response_to_client_message_id"], "client-msg-bravo",
        "correlation id must survive for reducer-layer bubble routing"
    );
}
