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
    data_dir: &Path,
    download_root: &Path,
    session_id: &SessionKey,
    default_thread_id: &str,
    msg: octos_core::OutboundMessage,
) -> Option<AssistantPersistOutcome> {
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
    store_delivered_media_copies(download_root, session_id, &msg.media).await;
    let persist = persist_assistant_with_media(
        sessions,
        data_dir,
        session_id,
        msg.content,
        msg.media,
        thread_id,
        "send_file",
    );
    if is_spawn_complete_companion {
        MESSAGE_PROJECTION_OVERRIDE
            .scope(Some(MessageProjectionOverride::Suppress), persist)
            .await
    } else {
        persist.await
    }
}

pub(super) struct AssistantPersistOutcome {
    pub(super) message: Message,
    /// `None` means the append committed but its sequence is unknown. This is
    /// deliberately distinct from the outer `Option`: outer `None` means the
    /// append did not commit and the caller may treat delivery as failed.
    pub(super) committed_seq: Option<usize>,
}

pub(super) fn assistant_persist_outcome_from_result(
    message: Message,
    result: eyre::Result<usize>,
) -> Option<AssistantPersistOutcome> {
    match result {
        Ok(seq) => Some(AssistantPersistOutcome {
            message,
            committed_seq: Some(seq),
        }),
        Err(error)
            if error
                .downcast_ref::<octos_bus::session::CommittedSeqUnknownError>()
                .is_some() =>
        {
            Some(AssistantPersistOutcome {
                message,
                committed_seq: None,
            })
        }
        Err(_) => None,
    }
}

pub(super) async fn persist_assistant_with_media(
    sessions: &Arc<TokioMutex<octos_bus::SessionManager>>,
    data_dir: &Path,
    session_id: &SessionKey,
    content: String,
    media: Vec<String>,
    thread_id: String,
    label: &str,
) -> Option<AssistantPersistOutcome> {
    let mut message = Message::assistant_with_thread(content, octos_core::ThreadId::new(thread_id));
    message.media = media;
    let committed_message = message.clone();
    let result =
        octos_bus::session::persist_message_through_canonical_path(data_dir, session_id, message)
            .await;
    if let Err(error) = &result {
        if error
            .downcast_ref::<octos_bus::session::CommittedSeqUnknownError>()
            .is_some()
        {
            tracing::warn!(
                session = %session_id.0,
                label,
                committed = true,
                seq_unknown = true,
                operation = "post_commit_readback",
                error = %error,
                "OUP persistence degraded: append committed; AssistantPersisted suppressed because committed_seq is unknown"
            );
        } else {
            tracing::warn!(
                session = %session_id.0,
                label,
                error = %error,
                "api/serve: failed to persist background-delivered message"
            );
        }
    }
    let outcome = assistant_persist_outcome_from_result(committed_message, result);
    if outcome.is_some() {
        // A committed-but-unnumbered row is still a durable delivery; invalidate
        // the read mirror without inventing a sequence or replaying the append.
        sessions.lock().await.invalidate_cache(session_id);
    }
    outcome
}
