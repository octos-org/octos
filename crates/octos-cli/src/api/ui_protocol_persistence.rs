//! Assistant persistence for background deliveries on the UI transport.

use super::*;

pub(super) fn record_appui_context_manager_background_message(
    data_dir: &Path,
    fallback_context_manager: &Arc<StdMutex<ContextManager>>,
    session_id: &SessionKey,
    message: &Message,
    seq: usize,
) {
    if let Some(live) = live_appui_session_context_manager(session_id) {
        let mut manager = live.lock().unwrap_or_else(|error| error.into_inner());
        merge_appui_out_of_band_row(&mut manager, session_id, message, seq, "background_result");
        publish_appui_context_status(session_id, &manager);
        if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
            warn!(
                session = %session_id.0,
                error = %error,
                "failed to persist appui background context boundary"
            );
        }
        return;
    }

    {
        let persist_lock = appui_context_persist_lock(session_id);
        let _persist_guard = persist_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match load_context_manager_snapshot(data_dir, &session_id.to_string()) {
            Ok(Some(mut manager)) => {
                merge_appui_out_of_band_row(
                    &mut manager,
                    session_id,
                    message,
                    seq,
                    "background_result",
                );
                publish_appui_context_status(session_id, &manager);
                if let Err(error) =
                    persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
                {
                    warn!(
                        session = %session_id.0,
                        error = %error,
                        "failed to persist appui background context boundary"
                    );
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    session = %session_id.0,
                    error = %error,
                    "context ledger snapshot unreadable; merging background row into the last per-turn manager"
                );
            }
        }
    }

    let mut manager = fallback_context_manager
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    merge_appui_out_of_band_row(&mut manager, session_id, message, seq, "background_result");
    publish_appui_context_status(session_id, &manager);
    if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui background context boundary"
        );
    }
}

/// Persist one `send_file` delivery (an `OutboundMessage` from the per-turn
/// `SendFileTool` channel) as an assistant message, after storing the browser
/// download copies of its files. A spawn-only companion is transcript-only: the linked v2
/// background-child payload owns its media and visible completion.
pub(super) async fn persist_send_file_delivery(
    sessions: &Arc<TokioMutex<octos_bus::SessionManager>>,
    download_root: &Path,
    session_id: &SessionKey,
    default_thread_id: &str,
    retention: octos_bus::session_artifacts::ArtifactRetentionPolicy,
    msg: octos_core::OutboundMessage,
) -> Option<(Message, usize)> {
    let thread_id = msg
        .metadata
        .get("thread_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_thread_id.to_string());
    let is_spawn_complete_companion = msg
        .metadata
        .get("spawn_complete_companion")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // A chat's files are served from its own folder by `/api/files`, which
    // checks the chat binding on every request (UPCR-2026-R1EFC56A3). A copy
    // would read each path again after `send_file` checked it against the
    // admitted folder, and a folder replaced in between would hand over
    // another file's bytes, so a chat, even an unusable one, gets no copies.
    let chat = crate::runtime::chat_workspace::lookup(download_root, session_id);
    if matches!(chat, Ok(None)) {
        store_delivered_media_copies(download_root, session_id, &msg.media, retention).await;
    }
    // Main's send_file path persists through the ACTIVE transcript manager
    // (not the canonical data_dir helper) so reload preserves
    // prompt/delivery/response order; a failure is soft — no wire artifact
    // references it, so the delivery simply isn't recorded.
    let persist = async {
        let mut message =
            Message::assistant_with_thread(msg.content, octos_core::ThreadId::new(thread_id));
        message.media = msg.media;
        let committed_message = message.clone();
        let committed_seq = sessions
            .lock()
            .await
            .add_message_with_seq(session_id, message)
            .await
            .inspect_err(|error| {
                tracing::warn!(session = %session_id.0, error = %error, "send_file persist failed");
            })
            .ok()?;
        Some((committed_message, committed_seq))
    };
    if is_spawn_complete_companion {
        MESSAGE_PROJECTION_OVERRIDE
            .scope(Some(MessageProjectionOverride::Suppress), persist)
            .await
    } else {
        persist.await
    }
}
