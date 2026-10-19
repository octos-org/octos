//! Batch the rebuildable context checkpoint after durable transcript appends.
//! Each successful row is still merged immediately and in sequence order.
//! Drop checkpoints a partial batch if the owning turn is cancelled/unwinds.

use crate::context_manager::ContextManager;
use octos_core::{Message, SessionKey};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_manager::{
        CompactContextPolicy, ContextLedgerLoadStatus, ContextManager, PromptBuildPolicy,
        context_ledger_path, load_context_manager_snapshot, load_or_rebuild_context_manager,
        persist_context_manager_snapshot,
    };
    use octos_bus::SessionManager;
    use octos_core::{Message, SessionKey, ThreadId, ToolCall};
    use std::sync::{Arc, Mutex};

    fn response_rows() -> Vec<Message> {
        let mut calls = Message::assistant("Inspecting the saved deck");
        calls.thread_id = Some("batch-thread".into());
        calls.tool_calls = Some(vec![ToolCall {
            id: "toolu_deck".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"deck.pptx"}),
            metadata: None,
        }]);
        vec![
            calls,
            Message::tool_with_thread("deck present", "toolu_deck", ThreadId::new("batch-thread")),
            Message::assistant_with_thread("Verified deck", ThreadId::new("batch-thread")),
        ]
    }

    fn prompt_value(manager: &ContextManager) -> serde_json::Value {
        let prompt = manager.for_prompt(&PromptBuildPolicy::default());
        // Compare semantic prompt fields, not fresh rendering timestamps.
        serde_json::json!(
            prompt
                .messages
                .iter()
                .map(|row| serde_json::json!({
                    "role":row.role, "content":row.content, "tool_calls":row.tool_calls,
                    "tool_call_id":row.tool_call_id, "media":row.media,
                }))
                .collect::<Vec<_>>()
        )
    }

    #[test]
    fn should_checkpoint_postturn_batch_once_with_same_normalized_projection() {
        let batched_dir = tempfile::tempdir().unwrap();
        let old_dir = tempfile::tempdir().unwrap();
        let session = SessionKey("batch-equivalence".into());
        let history = vec![Message::user("Check the deck")];
        let rows = response_rows();
        let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
        for row in &rows {
            initial.record_message(row);
        }
        let old = Arc::new(Mutex::new(initial.clone()));
        let manager = Arc::new(Mutex::new(initial));
        let path = context_ledger_path(batched_dir.path(), &session.to_string());
        let mut batch = AppUiContextCommitBatch::new(batched_dir.path(), &manager, &session);
        for (index, row) in rows.iter().enumerate() {
            let seq = history.len() + index;
            super::super::record_appui_context_manager_message(
                old_dir.path(),
                &old,
                &session,
                row,
                seq,
            );
            batch.record(row, seq);
            assert!(!path.exists(), "no full snapshot per response row");
            assert_eq!(manager.lock().unwrap().source_high_watermark(), Some(seq));
        }
        batch.finish().unwrap();
        let modified = path.metadata().unwrap().modified().unwrap();
        drop(batch);
        assert_eq!(
            path.metadata().unwrap().modified().unwrap(),
            modified,
            "drop must not checkpoint twice"
        );
        let loaded = load_context_manager_snapshot(batched_dir.path(), &session.to_string())
            .unwrap()
            .unwrap();
        let legacy = load_context_manager_snapshot(old_dir.path(), &session.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(prompt_value(&loaded), prompt_value(&legacy));
        assert_eq!(loaded.source_head_hash(), legacy.source_head_hash());
        assert_eq!(loaded.generation(), legacy.generation());
        assert_eq!(
            loaded.items().len(),
            legacy.items().len(),
            "in-flight twins must not duplicate"
        );
    }

    #[tokio::test]
    async fn should_checkpoint_completed_postturn_rows_when_task_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("batch-cancel".into());
        let manager = Arc::new(Mutex::new(ContextManager::new(session.to_string(), None)));
        let root = persist_context_manager_snapshot(
            dir.path(),
            &session.to_string(),
            &manager.lock().unwrap(),
        )
        .unwrap();
        let baseline = std::fs::read(&root).unwrap();
        let task_manager = manager.clone();
        let task_session = session.clone();
        let task_dir = dir.path().to_path_buf();
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut sessions = SessionManager::open(&task_dir).unwrap();
            let mut batch = AppUiContextCommitBatch::new(&task_dir, &task_manager, &task_session);
            let mut rows = vec![Message::user_rooting_thread(
                "Check deck",
                octos_core::ClientMessageId::new("batch-thread"),
            )];
            rows.extend(response_rows().into_iter().take(2));
            for row in rows {
                let seq = sessions
                    .add_message_with_seq(&task_session, row.clone())
                    .await
                    .unwrap();
                batch.record(&row, seq);
            }
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        received.await.unwrap();
        assert_eq!(std::fs::read(&root).unwrap(), baseline);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            std::fs::read(root).unwrap(),
            baseline,
            "cancellation appends a transition without rewriting history"
        );
        let loaded = load_context_manager_snapshot(dir.path(), &session.to_string())
            .unwrap()
            .unwrap();
        let mut sessions = SessionManager::open(dir.path()).unwrap();
        let history = sessions.get_or_create(&session).await.messages.clone();
        assert_eq!(history.len(), 3);
        let (recovered, status) =
            load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
        assert_eq!(status, ContextLedgerLoadStatus::Loaded);
        assert_eq!(loaded.source_head_hash(), recovered.source_head_hash());
        assert!(
            prompt_value(&recovered)
                .to_string()
                .contains("deck present")
        );
        assert!(
            !prompt_value(&recovered)
                .to_string()
                .contains("Verified deck")
        );
    }

    #[test]
    fn should_recover_postturn_journal_tail_before_batch_checkpoint_without_losing_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("batch-crash-recovery".into());
        let mut history = vec![
            Message::user("old ".repeat(1000)),
            Message::assistant("done ".repeat(1000)),
            Message::user("Check deck"),
        ];
        let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
        initial.compact_context(
            "earlier work saved",
            CompactContextPolicy {
                keep_recent_tokens: Some(64),
                target_tokens_after_compaction: Some(1000),
                ..Default::default()
            },
        );
        assert_eq!(initial.compactions().len(), 1);
        persist_context_manager_snapshot(dir.path(), &session.to_string(), &initial).unwrap();
        let manager = Arc::new(Mutex::new(initial));
        let mut batch = AppUiContextCommitBatch::new(dir.path(), &manager, &session);
        for row in response_rows().into_iter().take(2) {
            batch.record(&row, history.len());
            history.push(row);
        }
        // Reload ONLY disk snapshot + committed journal tail while the batch's
        // newer in-memory snapshot has not been checkpointed (process-death shape).
        let (recovered, status) =
            load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
        assert_eq!(status, ContextLedgerLoadStatus::Stale);
        assert_eq!(recovered.compactions().len(), 1);
        assert_eq!(
            prompt_value(&recovered),
            prompt_value(&manager.lock().unwrap())
        );
        assert_eq!(
            recovered.source_head_hash(),
            manager.lock().unwrap().source_head_hash()
        );
        batch.finish().unwrap();
    }

    #[test]
    fn should_recover_committed_postturn_rows_when_snapshot_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("batch-checkpoint-failure".into());
        let mut history = vec![Message::user("Check deck")];
        let manager = Arc::new(Mutex::new(ContextManager::from_session_history(
            session.to_string(),
            None,
            &history,
        )));
        let mut batch = AppUiContextCommitBatch::new(dir.path(), &manager, &session);
        for row in response_rows().into_iter().take(2) {
            batch.record(&row, history.len());
            history.push(row);
        }
        std::fs::write(
            dir.path().join("context_ledgers"),
            "blocked directory fixture",
        )
        .unwrap();
        assert!(batch.finish().is_err());
        drop(batch);
        let (recovered, _) =
            load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
        assert_eq!(
            prompt_value(&recovered),
            prompt_value(&manager.lock().unwrap())
        );
        assert!(
            !prompt_value(&recovered)
                .to_string()
                .contains("Verified deck")
        );
    }

    #[test]
    fn should_restart_incremental_batch_after_repeated_bridge_checkpoints_and_route_change() {
        use octos_agent::{PromptContextManager, PromptContextPhase, PromptContextRequest};
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("bridge-transition-restart".into());
        let mut history = vec![
            Message::user("earlier work"),
            Message::assistant("background result"),
            Message::user("finish verified work"),
        ];
        let initial_rows = history.len();
        let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
        initial.reconcile_prompt_cache_epoch("fixture", "model", "runtime system", &[]);
        let manager = Arc::new(Mutex::new(initial));
        let bridge = super::super::AppUiPromptContextBridge::new(
            session.clone(),
            dir.path().to_owned(),
            manager.clone(),
            false,
        );
        let request = |phase, iteration| PromptContextRequest {
            phase,
            iteration,
            provider_name: "fixture".into(),
            model_id: "model".into(),
            context_window: 200_000,
        };
        let mut prompt = vec![Message::system("runtime system")];
        prompt.extend(history.clone());
        bridge
            .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
            .unwrap();
        {
            let mut canonical = manager.lock().unwrap();
            let ids: Vec<_> = canonical
                .ledger_items()
                .iter()
                .filter(|item| {
                    item.source_ref
                        .as_ref()
                        .and_then(|source| source.source_seq)
                        == Some(1)
                })
                .map(|item| item.id.clone())
                .collect();
            let generation = canonical.generation();
            canonical.mark_source_event_kind(&ids, "background_result");
            assert_eq!(canonical.generation(), generation);
            super::super::persist_appui_context_snapshot(dir.path(), &session, &canonical).unwrap();
        }
        for iteration in 2..=4 {
            let mut call = Message::assistant("");
            let id = format!("call_bridge_{iteration}");
            call.tool_calls = Some(vec![ToolCall {
                id: id.clone(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md"}),
                metadata: None,
            }]);
            let result = Message::tool_with_thread("verified", id, ThreadId::new("thread"));
            history.extend([call.clone(), result.clone()]);
            prompt.extend([call, result]);
            let root = context_ledger_path(dir.path(), &session.to_string());
            let baseline = std::fs::read(&root).unwrap();
            bridge
                .prepare_prompt(
                    request(PromptContextPhase::Iteration, iteration),
                    &mut prompt,
                )
                .unwrap();
            assert!(
                std::fs::read(root).unwrap() == baseline,
                "ordinary iterations must append after adopting a background checkpoint or route change"
            );
            if iteration == 2 {
                bridge.observe_effective_provider_route("fixture", "model-2");
            }
        }
        let root = context_ledger_path(dir.path(), &session.to_string());
        let before = std::fs::read(&root).unwrap();
        let mut batch = AppUiContextCommitBatch::new(dir.path(), &manager, &session);
        for (index, row) in history.iter().enumerate().skip(initial_rows) {
            batch.record(row, index);
        }
        history.push(Message::assistant("done"));
        batch.record(history.last().unwrap(), history.len() - 1);
        batch.finish().unwrap();
        assert_eq!(
            std::fs::read(root).unwrap(),
            before,
            "last bridge checkpoint remains immutable"
        );
        let (loaded, status) =
            load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
        assert_eq!(status, ContextLedgerLoadStatus::Loaded);
        assert_eq!(
            prompt_value(&loaded),
            prompt_value(&manager.lock().unwrap())
        );
        assert_eq!(loaded.cache_epoch(), manager.lock().unwrap().cache_epoch());
        assert_eq!(
            loaded.source_head_hash(),
            manager.lock().unwrap().source_head_hash()
        );
        assert!(loaded.ledger_items().iter().any(|item| {
            item.source_ref.as_ref().is_some_and(|source| {
                source.source_seq == Some(1) && source.source_event_kind == "background_result"
            })
        }));
    }

    /// Provider-free diagnostic, intentionally excluded from normal test runs.
    /// Run with `benchmark_long_postturn_context_batch --ignored --nocapture`.
    #[test]
    #[ignore = "large synthetic history diagnostic; legacy checkpoint phase bounded to about 60s"]
    fn benchmark_long_postturn_context_batch() {
        let fixture_start = std::time::Instant::now();
        let legacy_dir = tempfile::tempdir().unwrap();
        let batch_dir = tempfile::tempdir().unwrap();
        let incremental_dir = tempfile::tempdir().unwrap();
        let session = SessionKey("benchmark-postturn-context".into());
        let stamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut history = vec![Message::user("Inspect the repository")];
        let payload = "stable source content; validated tool output; ".repeat(23);
        for index in 0..6_290 {
            let call_id = format!("call_history_{index}");
            let mut call = Message::assistant("");
            call.tool_calls = Some(vec![ToolCall {
                id: call_id.clone(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":format!("src/module_{index}.ts")}),
                metadata: None,
            }]);
            history.push(call);
            history.push(Message::tool_with_thread(
                format!("file {index}: {payload}"),
                call_id,
                ThreadId::new("history-thread"),
            ));
        }
        history.push(Message::user("Earlier repository inspection is complete"));
        for row in &mut history {
            row.timestamp = stamp;
        }
        let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
        let compaction = initial.compact_context(
            "Earlier repository inspection retained in summary",
            CompactContextPolicy {
                // Start at a call boundary: complete call/result pairs plus
                // the newest user row, then the installed summary itself.
                keep_recent_items: 2_901,
                ..Default::default()
            },
        );
        assert_eq!(
            compaction.status,
            crate::context_manager::ContextCompactionStatus::Installed
        );
        let initial_canonical_items = initial.ledger_items().len();
        let initial_active_items = initial.items().len();
        let mut rows = vec![Message::user("Finish the verified update")];
        for index in 0..5 {
            let call_id = format!("call_batch_{index}");
            let mut call = Message::assistant("");
            call.tool_calls = Some(vec![ToolCall {
                id: call_id.clone(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":format!("src/final_{index}.ts")}),
                metadata: None,
            }]);
            rows.push(call);
            rows.push(Message::tool_with_thread(
                format!("verified {index}: {payload}"),
                call_id,
                ThreadId::new("batch-thread"),
            ));
        }
        rows.push(Message::assistant(
            "All five checks completed; update verified.",
        ));
        for row in &mut rows {
            row.timestamp = stamp;
        }
        let legacy = Arc::new(Mutex::new(initial.clone()));
        let incremental = Arc::new(Mutex::new(initial.clone()));
        let incremental_root = persist_context_manager_snapshot(
            incremental_dir.path(),
            &session.to_string(),
            &incremental.lock().unwrap(),
        )
        .unwrap();
        let initial_checkpoint_bytes = std::fs::read(&incremental_root).unwrap();
        let batched = Arc::new(Mutex::new(initial));
        let fixture_seconds = fixture_start.elapsed().as_secs_f64();

        let legacy_path = context_ledger_path(legacy_dir.path(), &session.to_string());
        let legacy_start = std::time::Instant::now();
        let mut committed = Vec::new();
        let mut legacy_snapshot_installs = 0;
        let mut last_modified = None;
        for row in &rows {
            super::super::record_appui_context_manager_message(
                legacy_dir.path(),
                &legacy,
                &session,
                row,
                history.len() + committed.len(),
            );
            let modified = legacy_path.metadata().unwrap().modified().unwrap();
            assert_ne!(
                last_modified,
                Some(modified),
                "each legacy row installs another snapshot"
            );
            last_modified = Some(modified);
            legacy_snapshot_installs += 1;
            committed.push(row.clone());
            if legacy_start.elapsed().as_secs() >= 45 && row.role == octos_core::MessageRole::Tool {
                // Stop only after a complete tool pair, then persist the final
                // assistant row so both paths still see the same closed batch.
                let final_row = rows.last().unwrap();
                super::super::record_appui_context_manager_message(
                    legacy_dir.path(),
                    &legacy,
                    &session,
                    final_row,
                    history.len() + committed.len(),
                );
                legacy_snapshot_installs += 1;
                committed.push(final_row.clone());
                break;
            }
        }
        let legacy_seconds = legacy_start.elapsed().as_secs_f64();
        let batch_path = context_ledger_path(batch_dir.path(), &session.to_string());
        let batch_start = std::time::Instant::now();
        let mut batch = AppUiContextCommitBatch::new(batch_dir.path(), &batched, &session);
        for (index, row) in committed.iter().enumerate() {
            batch.record(row, history.len() + index);
            assert!(!batch_path.exists(), "no intermediate batch snapshot");
        }
        batch.finish().unwrap();
        let batch_seconds = batch_start.elapsed().as_secs_f64();
        let final_modified = batch_path.metadata().unwrap().modified().unwrap();
        drop(batch);
        assert_eq!(
            batch_path.metadata().unwrap().modified().unwrap(),
            final_modified
        );
        let incremental_start = std::time::Instant::now();
        let mut incremental_batch =
            AppUiContextCommitBatch::new(incremental_dir.path(), &incremental, &session);
        for (index, row) in committed.iter().enumerate() {
            incremental_batch.record(row, history.len() + index);
        }
        incremental_batch.finish().unwrap();
        let incremental_seconds = incremental_start.elapsed().as_secs_f64();
        assert_eq!(
            std::fs::read(&incremental_root).unwrap(),
            initial_checkpoint_bytes
        );
        let incremental_loaded =
            load_context_manager_snapshot(incremental_dir.path(), &session.to_string())
                .unwrap()
                .unwrap();
        let incremental_journal_bytes: u64 =
            std::fs::read_dir(incremental_dir.path().join("context_ledgers"))
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                })
                .map(|entry| entry.metadata().unwrap().len())
                .sum();
        let old = legacy.lock().unwrap();
        let new = batched.lock().unwrap();
        assert_eq!(prompt_value(&incremental_loaded), prompt_value(&new));
        assert_eq!(
            incremental_loaded.source_head_hash(),
            new.source_head_hash()
        );
        assert_eq!(prompt_value(&old), prompt_value(&new));
        assert_eq!(old.source_head_hash(), new.source_head_hash());
        let old_loaded = load_context_manager_snapshot(legacy_dir.path(), &session.to_string())
            .unwrap()
            .unwrap();
        let new_loaded = load_context_manager_snapshot(batch_dir.path(), &session.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(prompt_value(&old_loaded), prompt_value(&new_loaded));
        assert_eq!(old_loaded.source_head_hash(), new_loaded.source_head_hash());
        println!(
            "POSTTURN_BENCHMARK {}",
            serde_json::json!({
                "incremental_seconds":incremental_seconds,
                "incremental_journal_bytes":incremental_journal_bytes,
                "incremental_checkpoint_rewrites":0,
                "incremental_transition_appends":1,
                "incremental_reloaded_prompt_and_source_equal":true,
                "fixture_seconds":fixture_seconds,
                "history_rows":history.len(), "initial_canonical_items":initial_canonical_items,
                "initial_active_items":initial_active_items, "completed_response_rows":committed.len(),
                "planned_response_rows":rows.len(), "legacy_seconds":legacy_seconds,
                "batch_seconds":batch_seconds, "legacy_snapshot_installs":legacy_snapshot_installs,
                "batch_snapshot_installs":1, "legacy_snapshot_bytes":legacy_path.metadata().unwrap().len(),
                "batch_snapshot_bytes":batch_path.metadata().unwrap().len(),
                "final_canonical_items":new.ledger_items().len(), "final_active_items":new.items().len(),
                "normalized_prompt_equal":true, "source_head_equal":true,
                "reloaded_prompt_equal":true, "reloaded_source_head_equal":true,
                "profile":"debug", "order":"legacy_then_batch_then_incremental", "provider_calls":0,
                "scope":"one synthetic diagnostic under host contention; not a production speedup claim"
            })
        );
    }
}
