//! Once-per-session Conversation-episode handoff for the two ContextManager
//! bridges (issue #1620 / T-0405).
//!
//! The legacy `Agent` path persists the first compaction summary as an
//! `EpisodeSource::Conversation` episode; the serve (`AppUiPromptContextBridge`)
//! and gateway (`SessionActorPromptContextBridge`) bridges compacted context
//! WITHOUT that handoff, so their sessions never produced a Conversation
//! episode. This module is the shared implementation both bridges call after
//! an installed compaction.
//!
//! Idempotency: the episode id is deterministic — `conversation_<uuid-v5>`
//! over `(profile_id, session_key)` — and the existence check runs against
//! the durable store, not an in-memory latch, so reconnects, per-turn Agent
//! reconstruction (serve builds a fresh `Agent` per turn, resetting the
//! legacy `AtomicBool`), and process restarts cannot duplicate the episode.
//! The handoff never fails the turn: a store error is logged and retried on
//! the next compaction (no false ACK), mirroring the legacy path.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use octos_llm::{EmbeddingPriority, EmbeddingProvider};
use octos_memory::{Episode, EpisodeOutcome, EpisodeSource, EpisodeStore, RecallStore};
use tracing::{info, warn};

use crate::context_manager::{ContextCompactionStatus, ContextManager, TranscriptItemKind};
use octos_core::{AgentId, SessionKey, TaskId};

/// Memory wiring one bridge needs for the handoff: the session's own
/// episode/recall stores (namespace-resolved by the caller, so app-bound and
/// multi-tenant sessions keep their isolation), the profile embedder that
/// makes the episode recallable, and the session's workspace root.
#[derive(Clone)]
pub(crate) struct BridgeMemoryContext {
    pub profile_id: String,
    pub workspace_root: std::path::PathBuf,
    pub memory: Arc<EpisodeStore>,
    pub memory_enabled: bool,
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    pub recall: Option<Arc<RecallStore>>,
}

/// Deterministic episode id for a session's Conversation handoff. SHA-256
/// over the profile + session key + workspace root (truncated to 128 bits)
/// keeps profiles and workspaces isolated even when the legacy session id is
/// reused, and avoids adding a new uuid feature just for v5.
pub(crate) fn conversation_episode_id(
    profile_id: &str,
    session_key: &SessionKey,
    workspace_root: &Path,
) -> String {
    use sha2::{Digest, Sha256};
    let key = format!(
        "octos-conversation-episode-v1\u{1}{profile_id}\u{1}{}\u{1}{}",
        session_key.0,
        workspace_root.to_string_lossy()
    );
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("conversation_{hex}")
}

/// Hand off the authoritative compaction summary as a Conversation episode.
///
/// `compaction_performed` gates the call to an actually-installed compaction.
/// The summary text is read back from the compaction record's ledger item —
/// the SAME authoritative text the context ledger installed — rather than a
/// re-derived projection. `None`/no-op cases (no compaction, no summary row,
/// disabled memory, no embedder) return without side effects.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handoff_conversation_episode(
    manager: &Arc<StdMutex<ContextManager>>,
    session_key: &SessionKey,
    profile_id: &str,
    workspace_root: &Path,
    memory: &Arc<EpisodeStore>,
    memory_enabled: bool,
    embedder: &Option<Arc<dyn EmbeddingProvider>>,
    recall: &Option<Arc<RecallStore>>,
    compaction_performed: bool,
) {
    let guard = manager.lock().unwrap_or_else(|error| error.into_inner());
    handoff_conversation_episode_from_manager(
        &guard,
        session_key,
        profile_id,
        workspace_root,
        memory,
        memory_enabled,
        embedder,
        recall,
        compaction_performed,
    );
}

/// Direct-manager variant used by the pre-turn compaction producers. The
/// durable record must be `Installed`: a failed snapshot write must never
/// become the authoritative Conversation episode.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handoff_conversation_episode_from_manager(
    manager: &ContextManager,
    session_key: &SessionKey,
    profile_id: &str,
    workspace_root: &Path,
    memory: &Arc<EpisodeStore>,
    memory_enabled: bool,
    embedder: &Option<Arc<dyn EmbeddingProvider>>,
    recall: &Option<Arc<RecallStore>>,
    compaction_performed: bool,
) {
    if !compaction_performed || !memory_enabled || embedder.is_none() {
        return;
    }
    let (summary, episode_id) = {
        let Some(record) = manager.compactions().last() else {
            return;
        };
        if record.status != ContextCompactionStatus::Installed {
            return;
        }
        let Some(summary_item_id) = record.summary_item_id.as_ref() else {
            return;
        };
        let Some(summary) = manager.ledger_items().iter().find_map(|item| {
            if &item.id == summary_item_id {
                if let TranscriptItemKind::CompactionSummary { summary, .. } = &item.kind {
                    return Some(summary.clone());
                }
            }
            None
        }) else {
            return;
        };
        (
            summary,
            conversation_episode_id(profile_id, session_key, workspace_root),
        )
    };
    let summary = octos_core::truncated_utf8(&summary, 500, "...");
    if summary.trim().is_empty() {
        return;
    }
    let mut episode = Episode::new(
        TaskId::new(),
        AgentId(session_key.0.clone()),
        workspace_root.to_path_buf(),
        summary,
        EpisodeOutcome::Success,
    );
    episode.id = episode_id.clone();
    episode.source = EpisodeSource::Conversation;
    let memory = memory.clone();
    let embedder = embedder.clone();
    let recall = recall.clone();
    let session = session_key.0.clone();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            save_episode_once(episode, memory, embedder, recall, &session).await;
        });
    } else {
        warn!(session = %session, "no tokio runtime for conversation episode handoff; skipped (retry on next compaction)");
    }
}

/// Atomically insert-if-absent against the durable episode id. The returned
/// permit stays locked through recall and embedding; only full success calls
/// `complete`, so errors and process termination remain retryable rather than
/// being falsely acknowledged as a finished handoff.
async fn save_episode_once(
    episode: Episode,
    memory: Arc<EpisodeStore>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    recall: Option<Arc<RecallStore>>,
    session: &str,
) {
    let ep_id = episode.id.clone();
    let summary = episode.summary.clone();
    let mirror = octos_memory::record_from_episode(&episode);
    let permit = match memory.store_if_absent(episode).await {
        Ok(Some(permit)) => permit,
        Ok(None) => return, // already handed off (restart / reconnect / second bridge)
        Err(error) => {
            warn!(session = %session, error = %error, "failed to save conversation episode from context bridge; will retry on next compaction");
            return;
        }
    };
    let mut succeeded = true;
    if let Some(recall) = &recall {
        if let Err(error) = recall.upsert(vec![mirror], vec![None]) {
            warn!(session = %session, error = %error, "failed to mirror conversation episode into the recall index");
            succeeded = false;
        }
    }
    if let Some(embedder) = embedder {
        match embedder
            .embed_with_priority(&[summary.as_str()], EmbeddingPriority::Background)
            .await
        {
            Ok(vecs) => {
                if let Some(vec) = vecs.into_iter().next() {
                    if let Some(recall) = &recall {
                        if let Err(error) = recall.store_vector(&format!("episode:{ep_id}"), &vec) {
                            warn!(error = %error, "failed to mirror conversation-episode vector into recall");
                            succeeded = false;
                        }
                    }
                    if let Err(error) = memory.store_embedding(&ep_id, vec).await {
                        warn!(error = %error, "failed to store conversation-episode embedding");
                        succeeded = false;
                    }
                } else {
                    succeeded = false;
                }
            }
            Err(error) => {
                warn!(error = %error, "failed to embed conversation episode");
                succeeded = false;
            }
        }
    }
    if !succeeded {
        warn!(session = %session, episode_id = %ep_id, "conversation handoff incomplete; will retry on next compaction");
        return;
    }
    if let Err(error) = permit.complete() {
        warn!(session = %session, episode_id = %ep_id, error = %error, "failed to mark conversation handoff complete; will retry");
        return;
    }
    info!(session = %session, episode_id = %ep_id, "saved conversation episode from context bridge (#1620)");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::context_manager::{CompactContextPolicy, ContextManager};
    use octos_core::{Message, MessageRole};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) struct CountingEmbedder {
        pub(crate) calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        async fn embed(&self, texts: &[&str]) -> eyre::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }
        fn dimension(&self) -> usize {
            3
        }
    }

    pub(crate) fn conversation_messages() -> Vec<Message> {
        (0..12)
            .map(|index| Message {
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("turn {index}: a discussion about release planning"),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
                source: None,
                participant: None,
            })
            .collect()
    }

    pub(crate) fn manager_with_compaction(session: &str) -> Arc<StdMutex<ContextManager>> {
        let mut manager = ContextManager::new(session, None);
        for message in conversation_messages() {
            manager.record_message(&message);
        }
        manager.compact_context(
            "## Conversation Summary\n- release planning was discussed",
            CompactContextPolicy::default(),
        );
        Arc::new(StdMutex::new(manager))
    }

    fn session_key(raw: &str) -> SessionKey {
        SessionKey(raw.to_owned())
    }

    pub(crate) async fn wait_for_episode(store: &Arc<EpisodeStore>, episode_id: &str) -> Episode {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(episode) = store.get_by_id(episode_id).await.unwrap() {
                    return episode;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("conversation episode was not persisted within 30s")
    }

    pub(crate) async fn wait_for_embed_calls(embedder: &CountingEmbedder, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while embedder.calls.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("conversation episode embedding did not finish within 30s");
        assert_eq!(embedder.calls.load(Ordering::SeqCst), expected);
    }

    pub(crate) async fn wait_for_store_idle(store: &Arc<EpisodeStore>) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while Arc::strong_count(store) != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("conversation episode handoff tasks did not finish within 30s");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handoff_writes_one_episode_per_session_across_bridges_and_restarts() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("s1");
        let key = session_key("s1");
        let expected_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));

        // Two bridges (serve + gateway shapes), each handing off the same
        // session's compaction, twice (simulating a reconnect), plus a fresh
        // call after the store is re-read (simulating a restart): all must
        // converge on exactly one durable episode.
        for _ in 0..6 {
            handoff_conversation_episode(
                &manager,
                &key,
                "main",
                Path::new("/tmp/workspace"),
                &store,
                true,
                &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
                &None,
                true,
            );
        }
        let episode = wait_for_episode(&store, &expected_id).await;
        wait_for_store_idle(&store).await;
        assert_eq!(episode.source, EpisodeSource::Conversation);
        assert!(episode.summary.contains("release planning"));
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        // A REAL second compaction in the same session does NOT mint another
        // Conversation episode or replace the first authoritative summary.
        manager.lock().unwrap().compact_context(
            "second compaction must not replace the first episode",
            CompactContextPolicy {
                trigger: "second-compaction".to_owned(),
                ..CompactContextPolicy::default()
            },
        );
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace"),
            &store,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_store_idle(&store).await;
        let stored = store.get_by_id(&expected_id).await.unwrap().unwrap();
        assert!(stored.summary.contains("release planning"));
        assert!(!stored.summary.contains("second compaction"));
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        // A DIFFERENT session, profile, or workspace gets its own episode
        // (legacy ids alone are not an isolation boundary).
        let other = session_key("s2");
        let other_manager = manager_with_compaction("s2");
        handoff_conversation_episode(
            &other_manager,
            &other,
            "main",
            Path::new("/tmp/workspace"),
            &store,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        handoff_conversation_episode(
            &manager,
            &key,
            "profile-b",
            Path::new("/tmp/workspace"),
            &store,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace-b"),
            &store,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_store_idle(&store).await;
        assert!(
            store
                .get_by_id(&conversation_episode_id(
                    "main",
                    &other,
                    Path::new("/tmp/workspace")
                ))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_by_id(&conversation_episode_id(
                    "profile-b",
                    &key,
                    Path::new("/tmp/workspace")
                ))
                .await
                .unwrap()
                .is_some()
        );
        assert_ne!(
            conversation_episode_id("main", &key, Path::new("/tmp/workspace")),
            conversation_episode_id("profile-b", &key, Path::new("/tmp/workspace"))
        );
        let other_workspace_id =
            conversation_episode_id("main", &key, Path::new("/tmp/workspace-b"));
        assert_ne!(expected_id, other_workspace_id);
        assert!(
            store
                .get_by_id(&other_workspace_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handoff_is_a_noop_without_compaction_memory_or_embedder() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("s3");
        let key = session_key("s3");
        let expected_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));
        let embedder_dyn = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);

        // compaction_performed = false → no write.
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace"),
            &store,
            true,
            &embedder_dyn,
            &None,
            false,
        );
        // memory disabled → no write.
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace"),
            &store,
            false,
            &embedder_dyn,
            &None,
            true,
        );
        // no embedder → no write (unembedded episodes are invisible to recall).
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace"),
            &store,
            true,
            &None,
            &None,
            true,
        );
        assert!(store.get_by_id(&expected_id).await.unwrap().is_none());
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_handoffs_converge_on_one_episode() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("s4");
        let key = session_key("s4");
        let expected_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            let store = store.clone();
            let key = key.clone();
            let embedder = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);
            handles.push(tokio::spawn(async move {
                handoff_conversation_episode(
                    &manager,
                    &key,
                    "main",
                    Path::new("/tmp/workspace"),
                    &store,
                    true,
                    &embedder,
                    &None,
                    true,
                );
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        wait_for_episode(&store, &expected_id).await;
        wait_for_store_idle(&store).await;
        // At most one embed ran for the session's single episode; concurrent
        // duplicates collapse onto the same durable id.
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_turn_installed_compaction_is_handed_off_without_a_bridge_compaction() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("pre-turn-session");
        let key = session_key("pre-turn-session");
        let episode_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));
        {
            let guard = manager.lock().unwrap();
            handoff_conversation_episode_from_manager(
                &guard,
                &key,
                "main",
                Path::new("/tmp/workspace"),
                &store,
                true,
                &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
                &None,
                true,
            );
        }
        let episode = wait_for_episode(&store, &episode_id).await;
        wait_for_embed_calls(&embedder, 1).await;
        assert!(episode.summary.contains("release planning"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_compaction_persistence_is_not_handed_off() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("failed-compaction");
        manager
            .lock()
            .unwrap()
            .mark_last_compaction_failed("snapshot write failed".to_owned());
        let key = session_key("failed-compaction");
        let episode_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));
        {
            let guard = manager.lock().unwrap();
            handoff_conversation_episode_from_manager(
                &guard,
                &key,
                "main",
                Path::new("/tmp/workspace"),
                &store,
                true,
                &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
                &None,
                true,
            );
        }
        assert!(store.get_by_id(&episode_id).await.unwrap().is_none());
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn degraded_handoffs_deduplicate_recall_and_embed_work() {
        let temp = tempfile::tempdir().unwrap();
        let owner = Arc::new(EpisodeStore::open(temp.path()).await.unwrap());
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        assert!(degraded.is_degraded());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });
        let manager = manager_with_compaction("degraded-session");
        let key = session_key("degraded-session");
        for _ in 0..4 {
            handoff_conversation_episode(
                &manager,
                &key,
                "main",
                Path::new("/tmp/workspace"),
                &degraded,
                true,
                &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
                &None,
                true,
            );
        }
        wait_for_embed_calls(&embedder, 1).await;
        wait_for_store_idle(&degraded).await;
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
        drop(degraded);

        // A NEW degraded handle (gateway restart / second process) sees the
        // atomic claim file and must not repeat recall/embedding work.
        let reopened = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        assert!(reopened.is_degraded());
        for _ in 0..2 {
            handoff_conversation_episode(
                &manager,
                &key,
                "main",
                Path::new("/tmp/workspace"),
                &reopened,
                true,
                &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
                &None,
                true,
            );
        }
        wait_for_store_idle(&reopened).await;
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        // The canonical serve handle backfills the durable body behind the
        // degraded claim, without repeating recall/embedding work.
        let episode_id = conversation_episode_id("main", &key, Path::new("/tmp/workspace"));
        handoff_conversation_episode(
            &manager,
            &key,
            "main",
            Path::new("/tmp/workspace"),
            &owner,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_store_idle(&owner).await;
        assert!(owner.get_by_id(&episode_id).await.unwrap().is_some());
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        // Reverse order: canonical first, then degraded. The same claim must
        // suppress the degraded recall/embedding pipeline as well.
        let canonical_manager = manager_with_compaction("canonical-first");
        let canonical_key = session_key("canonical-first");
        let canonical_id =
            conversation_episode_id("main", &canonical_key, Path::new("/tmp/workspace"));
        handoff_conversation_episode(
            &canonical_manager,
            &canonical_key,
            "main",
            Path::new("/tmp/workspace"),
            &owner,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_episode(&owner, &canonical_id).await;
        wait_for_embed_calls(&embedder, 2).await;
        wait_for_store_idle(&owner).await;
        handoff_conversation_episode(
            &canonical_manager,
            &canonical_key,
            "main",
            Path::new("/tmp/workspace"),
            &reopened,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_store_idle(&reopened).await;
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incomplete_handoffs_are_retried_after_permit_drop() {
        use std::path::PathBuf;

        let temp = tempfile::tempdir().unwrap();
        let owner = Arc::new(EpisodeStore::open(temp.path()).await.unwrap());
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        let embedder = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
        });

        // Simulate process/task termination AFTER the canonical DB write but
        // BEFORE recall/embed and completion: dropping the permit publishes
        // no done marker, so a reopened handle must retry downstream work.
        let canonical_manager = manager_with_compaction("canonical-interrupted");
        let canonical_key = session_key("canonical-interrupted");
        let canonical_episode = {
            let mut episode = Episode::new(
                TaskId::new(),
                AgentId(canonical_key.0.clone()),
                PathBuf::from("/tmp/workspace"),
                "canonical interrupted summary".to_owned(),
                EpisodeOutcome::Success,
            );
            episode.id =
                conversation_episode_id("main", &canonical_key, Path::new("/tmp/workspace"));
            episode.source = EpisodeSource::Conversation;
            episode
        };
        let interrupted = owner
            .store_if_absent(canonical_episode)
            .await
            .unwrap()
            .unwrap();
        drop(interrupted);
        drop(owner);
        let owner = Arc::new(EpisodeStore::open(temp.path()).await.unwrap());
        handoff_conversation_episode(
            &canonical_manager,
            &canonical_key,
            "main",
            Path::new("/tmp/workspace"),
            &owner,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_embed_calls(&embedder, 1).await;
        wait_for_store_idle(&owner).await;

        // Same interruption on a degraded handle: the lock is released and no
        // completion marker exists, so the next degraded handle retries.
        let degraded_manager = manager_with_compaction("degraded-interrupted");
        let degraded_key = session_key("degraded-interrupted");
        let degraded_episode = {
            let mut episode = Episode::new(
                TaskId::new(),
                AgentId(degraded_key.0.clone()),
                PathBuf::from("/tmp/workspace"),
                "degraded interrupted summary".to_owned(),
                EpisodeOutcome::Success,
            );
            episode.id =
                conversation_episode_id("main", &degraded_key, Path::new("/tmp/workspace"));
            episode.source = EpisodeSource::Conversation;
            episode
        };
        let interrupted = degraded
            .store_if_absent(degraded_episode)
            .await
            .unwrap()
            .unwrap();
        drop(interrupted);
        drop(degraded);
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        handoff_conversation_episode(
            &degraded_manager,
            &degraded_key,
            "main",
            Path::new("/tmp/workspace"),
            &degraded,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_embed_calls(&embedder, 2).await;
        wait_for_store_idle(&degraded).await;

        // Completed retries are idempotent on the next handoff.
        handoff_conversation_episode(
            &canonical_manager,
            &canonical_key,
            "main",
            Path::new("/tmp/workspace"),
            &owner,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        handoff_conversation_episode(
            &degraded_manager,
            &degraded_key,
            "main",
            Path::new("/tmp/workspace"),
            &degraded,
            true,
            &Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
            &None,
            true,
        );
        wait_for_store_idle(&owner).await;
        wait_for_store_idle(&degraded).await;
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
impl tests::CountingEmbedder {
    fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_survives_real_store_reopen_without_duplicate() {
    use crate::conversation_episode::tests::{
        CountingEmbedder, manager_with_compaction, wait_for_store_idle,
    };
    use std::sync::atomic::Ordering;

    let temp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(octos_memory::EpisodeStore::open(temp.path()).await.unwrap());
    let embedder = std::sync::Arc::new(CountingEmbedder::new());
    let manager = manager_with_compaction("restart-session");
    let key = SessionKey("restart-session".to_owned());
    let episode_id = conversation_episode_id("main", &key, temp.path());
    let embedder_dyn = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);
    handoff_conversation_episode(
        &manager,
        &key,
        "main",
        temp.path(),
        &store,
        true,
        &embedder_dyn,
        &None,
        true,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.get_by_id(&episode_id).await.unwrap().is_none()
            || std::sync::Arc::strong_count(&store) != 1
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    drop(store);

    // Reopen the same redb file as a new process would, then repeat the
    // handoff: the durable deterministic id suppresses a second write/embed.
    let reopened =
        std::sync::Arc::new(octos_memory::EpisodeStore::open(temp.path()).await.unwrap());
    handoff_conversation_episode(
        &manager,
        &key,
        "main",
        temp.path(),
        &reopened,
        true,
        &embedder_dyn,
        &None,
        true,
    );
    wait_for_store_idle(&reopened).await;
    assert!(reopened.get_by_id(&episode_id).await.unwrap().is_some());
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
}
