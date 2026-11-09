//! Batch the rebuildable context checkpoint after durable transcript appends.
//! Each successful row is still merged immediately and in sequence order.
//! Drop checkpoints a partial batch if the owning turn is cancelled/unwinds.

use crate::context_manager::ContextManager;
use octos_core::{Message, MessageRole, SessionKey};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Commit an interrupted turn's dispatched prompt to durable history.
///
/// A turn's rows reach the canonical transcript only in the agent task's
/// end-of-turn persist loop; an interrupt aborts that task first, so the
/// dispatched, acknowledged prompt used to vanish from durable history and
/// the context ledger then discarded its source-less twin on reload
/// (#2831). Called before the terminal emits on interrupt, connection-close,
/// provider-error and lost-terminal paths (#2881).
///
/// The dying task persists `response.messages` under the same session
/// manager lock, so the lock serializes the two live writers: any User row
/// stamped with this turn's thread means the prompt (always that loop's
/// first row) is already durable. The prompt is persisted as plain text —
/// a media turn interrupted mid-flight keeps its text but not its
/// attachments, which only the never-run persist loop would have written.
/// Internal master continuations never persist their prompt (the
/// transport's `skip_internal_user_persist`) and are excluded by the
/// caller. Returns the committed sequence, or `None` when the row was
/// already durable or the append failed (logged).
pub(super) async fn persist_interrupted_turn_prompt(
    sessions: &Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    session: &SessionKey,
    turn_thread_id: &str,
    prompt: &str,
    data_dir: &Path,
    manager: &Arc<Mutex<ContextManager>>,
) -> Option<usize> {
    match persist_turn_prompt(
        sessions,
        session,
        turn_thread_id,
        Message::user(prompt),
        data_dir,
        manager,
        true,
    )
    .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::warn!(session = %session.0, %error, "failed to persist the dispatched prompt");
            None
        }
    }
}

/// Persist the dispatched user message before model execution. The same
/// append serves abnormal-exit recovery with `recover_missing_only`; media is
/// retained on normal dispatch. A new dispatch must append even when a client
/// reused a previous turn id. Failure prevents execution of an unrecorded request.
pub(super) async fn persist_turn_prompt(
    sessions: &Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    session: &SessionKey,
    turn_thread_id: &str,
    message: Message,
    data_dir: &Path,
    manager: &Arc<Mutex<ContextManager>>,
    recover_missing_only: bool,
) -> eyre::Result<Option<usize>> {
    let mut sessions = sessions.lock().await;
    if recover_missing_only
        && sessions
            .get_or_create(session)
            .await
            .messages
            .iter()
            .any(|message| {
                message.role == MessageRole::User
                    && message.thread_id.as_deref() == Some(turn_thread_id)
            })
    {
        return Ok(None);
    }
    let to_save = super::pre_stamp_turn_thread_id(message, turn_thread_id);
    let seq = sessions
        .add_message_with_seq(session, to_save.clone())
        .await?;
    let mut batch = AppUiContextCommitBatch::new(data_dir, manager, session);
    batch.record(&to_save, seq);
    if let Err(error) = batch.finish() {
        tracing::warn!(session = %session.0, seq, %error,
            "prompt committed but the rebuildable context checkpoint failed");
    }
    Ok(Some(seq))
}

pub(super) struct AppUiContextCommitBatch {
    data_dir: PathBuf,
    manager: Arc<Mutex<ContextManager>>,
    session: SessionKey,
    pending_rows: usize,
}

impl AppUiContextCommitBatch {
    pub(super) fn new(
        data_dir: &Path,
        manager: &Arc<Mutex<ContextManager>>,
        session: &SessionKey,
    ) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            manager: manager.clone(),
            session: session.clone(),
            pending_rows: 0,
        }
    }

    /// Call only after the canonical transcript append returned its sequence.
    /// No mutex guard is retained across the next asynchronous journal append.
    pub(super) fn record(&mut self, message: &Message, seq: usize) {
        self.pending_rows += 1;
        self.manager
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record_persisted_message_merging_prompt_equivalent(message, seq);
    }

    /// Finish before releasing the session lock or emitting a terminal result.
    /// A failed derived checkpoint leaves the canonical journal authoritative,
    /// as on the existing per-row path; reload can reconcile its appended tail.
    pub(super) fn finish(&mut self) -> Result<(), String> {
        let rows = std::mem::take(&mut self.pending_rows);
        if rows == 0 {
            return Ok(());
        }
        let start = std::time::Instant::now();
        let manager = self
            .manager
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        super::publish_appui_context_status(&self.session, &manager);
        let result =
            super::persist_appui_context_transition(&self.data_dir, &self.session, &manager)
                .map(|_| ());
        tracing::info!(
            session = %self.session.0,
            committed_rows = rows,
            checkpoint_ms = start.elapsed().as_millis(),
            checkpoint_ok = result.is_ok(),
            "post-turn context batch checkpoint finished"
        );
        result
    }
}

impl Drop for AppUiContextCommitBatch {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::warn!(session = %self.session.0, %error, "failed to checkpoint partial post-turn context batch");
        }
    }
}
