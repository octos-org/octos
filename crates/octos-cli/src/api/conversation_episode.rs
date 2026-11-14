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
    if !compaction_performed || !memory_enabled {
        return;
    }
    // No embedder, no handoff, so every completed handoff records a vector.
    let Some(embedder) = embedder.clone() else {
        return;
    };
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
    embedder: Arc<dyn EmbeddingProvider>,
    recall: Option<Arc<RecallStore>>,
    session: &str,
) {
    let permit = match memory.store_if_absent(episode, recall.clone()).await {
        Ok(Some(permit)) => permit,
        Ok(None) => return, // already handed off (restart / reconnect / second bridge)
        Err(error) => {
            warn!(session = %session, error = %error, "failed to save conversation episode from context bridge; will retry on next compaction");
            return;
        }
    };
    // The permit's episode is the first claim, which may be an earlier
    // compaction than this one: recall and the vector follow its text.
    let ep_id = permit.episode().id.clone();
    let summary = permit.episode().summary.clone();
    let mut succeeded = true;
    if let Some(recall) = &recall {
        let mirror = octos_memory::record_from_episode(permit.episode());
        if let Err(error) = recall.upsert(vec![mirror], vec![None]) {
            warn!(session = %session, error = %error, "failed to mirror conversation episode into the recall index");
            succeeded = false;
        }
    }
    // The embedder's identity as the profile's recall records it; a vector
    // cached by another embedder or at another width is embedded again.
    let embedder_id = recall.as_ref().map(|r| r.embedder_id()).unwrap_or_default();
    let vector = match permit.completed_vector(&embedder_id, embedder.dimension()) {
        Some(vector) => Some(vector.to_vec()),
        None => match embedder
            .embed_with_priority(&[summary.as_str()], EmbeddingPriority::Background)
            .await
        {
            Ok(vecs) => vecs.into_iter().next(),
            Err(error) => {
                warn!(error = %error, "failed to embed conversation episode");
                None
            }
        },
    };
    let Some(vector) = vector else {
        warn!(session = %session, episode_id = %ep_id, "conversation handoff incomplete; will retry on next compaction");
        return;
    };
    if let Some(recall) = &recall {
        if let Err(error) = recall.store_vector(&format!("episode:{ep_id}"), &vector) {
            warn!(error = %error, "failed to mirror conversation-episode vector into recall");
            succeeded = false;
        }
    }
    if let Err(error) = memory.store_embedding(&ep_id, vector.clone()).await {
        warn!(error = %error, "failed to store conversation-episode embedding");
        succeeded = false;
    }
    if !succeeded {
        warn!(session = %session, episode_id = %ep_id, "conversation handoff incomplete; will retry on next compaction");
        return;
    }
    if let Err(error) = permit.complete(&vector, &embedder_id) {
        warn!(session = %session, episode_id = %ep_id, error = %error, "failed to mark conversation handoff complete; will retry");
        return;
    }
    info!(session = %session, episode_id = %ep_id, "saved conversation episode from context bridge (#1620)");
}

#[cfg(test)]
#[path = "conversation_episode_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{
        CountingEmbedder, assert_indexes_agree, canonical_recall, canonical_recall_at,
        conversation_messages, degraded_recall, wait_for_embed_calls, wait_for_episode,
        wait_for_store_idle,
    };
    use super::*;
    use crate::context_manager::CompactContextPolicy;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const WORKSPACE: &str = "/tmp/workspace";

    fn manager_with_compaction(session: &str) -> Arc<StdMutex<ContextManager>> {
        manager_with_summary(
            session,
            "## Conversation Summary\n- release planning was discussed",
        )
    }

    fn manager_with_summary(session: &str, summary: &str) -> Arc<StdMutex<ContextManager>> {
        let mut manager = ContextManager::new(session, None);
        for message in conversation_messages() {
            manager.record_message(&message);
        }
        manager.compact_context(summary, CompactContextPolicy::default());
        Arc::new(StdMutex::new(manager))
    }

    fn session_key(raw: &str) -> SessionKey {
        SessionKey(raw.to_owned())
    }

    /// The episode id of `key` under profile `main` in [`WORKSPACE`].
    fn episode_id(key: &SessionKey) -> String {
        conversation_episode_id("main", key, Path::new(WORKSPACE))
    }

    fn calls(embedder: &CountingEmbedder) -> usize {
        embedder.calls.load(Ordering::SeqCst)
    }

    /// The canonical store at [`CountingEmbedder`]'s width, so its vectors
    /// count as present rather than mismatched.
    async fn open_owner(dir: &Path) -> Arc<EpisodeStore> {
        Arc::new(EpisodeStore::open_with_dimension(dir, 3).await.unwrap())
    }

    async fn stored(store: &Arc<EpisodeStore>, id: &str) -> bool {
        store.get_by_id(id).await.unwrap().is_some()
    }

    /// Hand `manager`'s compaction off as a bridge does, under `(profile,
    /// workspace)`: memory on, the counting embedder, no recall, a compaction
    /// performed this turn.
    fn handoff_at(
        manager: &Arc<StdMutex<ContextManager>>,
        key: &SessionKey,
        (profile, workspace): (&str, &Path),
        store: &Arc<EpisodeStore>,
        embedder: &Arc<CountingEmbedder>,
    ) {
        let embedder = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);
        handoff_conversation_episode(
            manager, key, profile, workspace, store, true, &embedder, &None, true,
        );
    }

    /// [`handoff`] with a canonical recall index.
    fn handoff_with_recall(
        manager: &Arc<StdMutex<ContextManager>>,
        key: &SessionKey,
        store: &Arc<EpisodeStore>,
        embedder: &Arc<CountingEmbedder>,
        recall: &Arc<RecallStore>,
    ) {
        let embedder = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);
        let (workspace, recall) = (Path::new(WORKSPACE), Some(recall.clone()));
        handoff_conversation_episode(
            manager, key, "main", workspace, store, true, &embedder, &recall, true,
        );
    }

    /// [`handoff_at`] under profile `main` in [`WORKSPACE`].
    fn handoff(
        manager: &Arc<StdMutex<ContextManager>>,
        key: &SessionKey,
        store: &Arc<EpisodeStore>,
        embedder: &Arc<CountingEmbedder>,
    ) {
        handoff_at(
            manager,
            key,
            ("main", Path::new(WORKSPACE)),
            store,
            embedder,
        );
    }

    /// The pre-turn producer's handoff, from the locked manager.
    fn handoff_from_manager(
        manager: &Arc<StdMutex<ContextManager>>,
        key: &SessionKey,
        store: &Arc<EpisodeStore>,
        embedder: &Arc<CountingEmbedder>,
    ) {
        let embedder = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);
        let (guard, workspace) = (manager.lock().unwrap(), Path::new(WORKSPACE));
        handoff_conversation_episode_from_manager(
            &guard, key, "main", workspace, store, true, &embedder, &None, true,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handoff_writes_one_episode_per_session_across_bridges_and_restarts() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder::new());
        let manager = manager_with_compaction("s1");
        let key = session_key("s1");
        let expected_id = episode_id(&key);

        // Two bridges (serve + gateway shapes), each handing off the same
        // session's compaction, twice (simulating a reconnect), plus a fresh
        // call after the store is re-read (simulating a restart): all must
        // converge on exactly one durable episode.
        for _ in 0..6 {
            handoff(&manager, &key, &store, &embedder);
        }
        let episode = wait_for_episode(&store, &expected_id).await;
        wait_for_store_idle(&store).await;
        assert_eq!(episode.source, EpisodeSource::Conversation);
        assert!(episode.summary.contains("release planning"));
        assert_eq!(calls(&embedder), 1);

        // A REAL second compaction in the same session does NOT mint another
        // Conversation episode or replace the first authoritative summary.
        manager.lock().unwrap().compact_context(
            "second compaction must not replace the first episode",
            CompactContextPolicy {
                trigger: "second-compaction".to_owned(),
                ..CompactContextPolicy::default()
            },
        );
        handoff(&manager, &key, &store, &embedder);
        wait_for_store_idle(&store).await;
        let first = store.get_by_id(&expected_id).await.unwrap().unwrap();
        assert!(first.summary.contains("release planning"));
        assert!(!first.summary.contains("second compaction"));
        assert_eq!(calls(&embedder), 1);

        // A DIFFERENT session, profile, or workspace gets its own episode
        // (legacy ids alone are not an isolation boundary).
        let other = session_key("s2");
        handoff(&manager_with_compaction("s2"), &other, &store, &embedder);
        let (workspace, workspace_b) = (Path::new(WORKSPACE), Path::new("/tmp/workspace-b"));
        handoff_at(&manager, &key, ("profile-b", workspace), &store, &embedder);
        handoff_at(&manager, &key, ("main", workspace_b), &store, &embedder);
        wait_for_store_idle(&store).await;
        let profile_b_id = conversation_episode_id("profile-b", &key, workspace);
        let other_workspace_id = conversation_episode_id("main", &key, workspace_b);
        assert_ne!(expected_id, profile_b_id);
        assert_ne!(expected_id, other_workspace_id);
        for id in [episode_id(&other), profile_b_id, other_workspace_id] {
            assert!(stored(&store, &id).await, "{id}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handoff_is_a_noop_without_compaction_memory_or_embedder() {
        let temp = tempfile::tempdir().unwrap();
        let store = open_owner(temp.path()).await;
        let embedder = Arc::new(CountingEmbedder::new());
        let manager = manager_with_compaction("s3");
        let key = session_key("s3");
        let embedder_dyn = Some(embedder.clone() as Arc<dyn EmbeddingProvider>);

        // (memory enabled, embedder, compaction performed): no compaction
        // this turn, memory disabled, or no embedder each write nothing.
        for (memory_enabled, embedder, compacted) in [
            (true, &embedder_dyn, false),
            (false, &embedder_dyn, true),
            (true, &None, true),
        ] {
            let workspace = Path::new(WORKSPACE);
            handoff_conversation_episode(
                &manager,
                &key,
                "main",
                workspace,
                &store,
                memory_enabled,
                embedder,
                &None,
                compacted,
            );
        }
        assert!(!stored(&store, &episode_id(&key)).await);
        assert_eq!(calls(&embedder), 0);

        // Nor a claim or marker: the first handoff with an embedder runs.
        handoff(&manager, &key, &store, &embedder);
        wait_for_episode(&store, &episode_id(&key)).await;
        wait_for_embed_calls(&embedder, 1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_handoffs_converge_on_one_episode() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder::new());
        let manager = manager_with_compaction("s4");
        let key = session_key("s4");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (manager, store) = (manager.clone(), store.clone());
            let (key, embedder) = (key.clone(), embedder.clone());
            handles.push(tokio::spawn(async move {
                handoff(&manager, &key, &store, &embedder);
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        wait_for_episode(&store, &episode_id(&key)).await;
        wait_for_store_idle(&store).await;
        // At most one embed ran for the session's single episode; concurrent
        // duplicates collapse onto the same durable id.
        assert_eq!(calls(&embedder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_turn_handoff_takes_only_an_installed_compaction() {
        let store = Arc::new(EpisodeStore::in_memory().unwrap());
        let embedder = Arc::new(CountingEmbedder::new());
        // The pre-turn producer hands off an installed compaction even when
        // no in-loop bridge compaction fires.
        let manager = manager_with_compaction("pre-turn-session");
        let key = session_key("pre-turn-session");
        handoff_from_manager(&manager, &key, &store, &embedder);
        let episode = wait_for_episode(&store, &episode_id(&key)).await;
        wait_for_embed_calls(&embedder, 1).await;
        assert!(episode.summary.contains("release planning"));

        // A compaction whose durable snapshot failed is not handed off.
        let manager = manager_with_compaction("failed-compaction");
        let failure = "snapshot write failed".to_owned();
        manager.lock().unwrap().mark_last_compaction_failed(failure);
        let key = session_key("failed-compaction");
        handoff_from_manager(&manager, &key, &store, &embedder);
        wait_for_store_idle(&store).await;
        assert!(!stored(&store, &episode_id(&key)).await);
        assert_eq!(calls(&embedder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn degraded_handoffs_deduplicate_recall_and_embed_work() {
        let temp = tempfile::tempdir().unwrap();
        let owner = open_owner(temp.path()).await;
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        assert!(degraded.is_degraded());
        let embedder = Arc::new(CountingEmbedder::new());
        let manager = manager_with_compaction("degraded-session");
        let key = session_key("degraded-session");
        for _ in 0..4 {
            handoff(&manager, &key, &degraded, &embedder);
        }
        wait_for_embed_calls(&embedder, 1).await;
        wait_for_store_idle(&degraded).await;
        assert_eq!(calls(&embedder), 1);
        drop(degraded);

        // A NEW degraded handle (gateway restart / second process) sees the
        // atomic claim file and must not repeat recall/embedding work.
        let reopened = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        assert!(reopened.is_degraded());
        for _ in 0..2 {
            handoff(&manager, &key, &reopened, &embedder);
        }
        wait_for_store_idle(&reopened).await;
        assert_eq!(calls(&embedder), 1);

        // The canonical serve handle, with a later and different compaction,
        // makes the degraded claim durable: its body, its recall record and
        // the vector the degraded handoff recorded, with no second embedding.
        let recall = canonical_recall(temp.path());
        let later = manager_with_summary("degraded-session", "a later, different compaction");
        handoff_with_recall(&later, &key, &owner, &embedder, &recall);
        wait_for_store_idle(&owner).await;
        assert_eq!(calls(&embedder), 1);
        let claimed = &embedder.texts()[0];
        assert!(claimed.contains("release planning"));
        assert_indexes_agree(&owner, &recall, &episode_id(&key), claimed).await;

        // Reverse order: canonical first, then degraded. The same claim must
        // suppress the degraded recall/embedding pipeline as well.
        let canonical_manager = manager_with_compaction("canonical-first");
        let canonical_key = session_key("canonical-first");
        handoff(&canonical_manager, &canonical_key, &owner, &embedder);
        wait_for_episode(&owner, &episode_id(&canonical_key)).await;
        wait_for_embed_calls(&embedder, 2).await;
        wait_for_store_idle(&owner).await;
        handoff(&canonical_manager, &canonical_key, &reopened, &embedder);
        wait_for_store_idle(&reopened).await;
        assert_eq!(calls(&embedder), 2);

        // Damaged handoff files never skip the backfill. A corrupt claim is
        // replaced: the degraded handle claims its own text, and the canonical
        // one restores its redb row, the durable body. A done marker with no
        // usable vector (an earlier build's bare one, or a corrupt one) makes
        // the canonical handle embed again.
        let legacy = manager_with_compaction("legacy-marker");
        let legacy_key = session_key("legacy-marker");
        let id = episode_id(&legacy_key);
        store_interrupted(&owner, &legacy_key, "canonical body").await;
        let markers = temp.path().join("conversation-episode-handoff");
        std::fs::write(markers.join(format!("{id}.claim.json")), b"{").unwrap();
        handoff(&legacy, &legacy_key, &reopened, &embedder);
        wait_for_embed_calls(&embedder, 3).await;
        wait_for_store_idle(&reopened).await;
        std::fs::write(markers.join(format!("{id}.done")), b"done\n").unwrap();
        handoff(&legacy, &legacy_key, &reopened, &embedder);
        handoff_with_recall(&legacy, &legacy_key, &owner, &embedder, &recall);
        wait_for_store_idle(&reopened).await;
        wait_for_store_idle(&owner).await;
        assert_eq!(calls(&embedder), 4);
        assert_indexes_agree(&owner, &recall, &id, "canonical body").await;

        // A canonical episode store whose recall fell back to memory (another
        // process holds recall.redb) completes with only the episode store
        // durable. The canonical recall handle still backfills the record
        // with the saved vector, and it survives a reopen.
        let split = manager_with_compaction("split-recall");
        let split_key = session_key("split-recall");
        let id = episode_id(&split_key);
        let memory_recall = degraded_recall(temp.path());
        handoff_with_recall(&split, &split_key, &owner, &embedder, &memory_recall);
        wait_for_embed_calls(&embedder, 5).await;
        wait_for_store_idle(&owner).await;
        handoff_with_recall(&split, &split_key, &owner, &embedder, &recall);
        wait_for_store_idle(&owner).await;
        assert_eq!(calls(&embedder), 5);
        let text = embedder.texts()[4].clone();
        assert_indexes_agree(&owner, &recall, &id, &text).await;
        drop(recall);
        let recall = canonical_recall(temp.path());
        assert_indexes_agree(&owner, &recall, &id, &text).await;
        handoff_with_recall(&split, &split_key, &owner, &embedder, &recall);
        wait_for_store_idle(&owner).await;
        assert_eq!(calls(&embedder), 5);

        // The profile switches to a 4-wide embedder. The stored and saved
        // vectors are 3 wide: the reopened episode store embeds the claimed
        // text again rather than reuse them, the canonical recall then takes
        // that new vector without embedding, and a later compaction does
        // nothing.
        drop((owner, recall));
        let owner = EpisodeStore::open_with_dimension(temp.path(), 4).await;
        let (owner, recall) = (
            Arc::new(owner.unwrap()),
            canonical_recall_at(temp.path(), 4),
        );
        let wider = Arc::new(CountingEmbedder::with_dimension(4));
        handoff(&split, &split_key, &owner, &wider);
        wait_for_store_idle(&owner).await;
        assert_eq!(wider.texts(), std::slice::from_ref(&text));
        for _ in 0..2 {
            handoff_with_recall(&split, &split_key, &owner, &wider, &recall);
            wait_for_store_idle(&owner).await;
        }
        assert_eq!(calls(&wider), 1);
        assert_indexes_agree(&owner, &recall, &id, &text).await;
    }

    /// Store `key`'s conversation episode and drop its permit with no
    /// recall, embedding or completion marker, as when the process dies right
    /// after the canonical write.
    async fn store_interrupted(store: &Arc<EpisodeStore>, key: &SessionKey, summary: &str) {
        let (task, agent, workspace) = (TaskId::new(), AgentId(key.0.clone()), WORKSPACE);
        let outcome = EpisodeOutcome::Success;
        let mut episode = Episode::new(
            task,
            agent,
            PathBuf::from(workspace),
            summary.into(),
            outcome,
        );
        episode.id = episode_id(key);
        episode.source = EpisodeSource::Conversation;
        drop(store.store_if_absent(episode, None).await.unwrap().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incomplete_handoffs_are_retried_after_permit_drop() {
        let temp = tempfile::tempdir().unwrap();
        let owner = open_owner(temp.path()).await;
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        let embedder = Arc::new(CountingEmbedder::new());

        // Simulate process/task termination AFTER the canonical DB write but
        // BEFORE recall/embed and completion: dropping the permit publishes
        // no done marker, so a reopened handle must retry downstream work.
        let canonical_manager = manager_with_compaction("canonical-interrupted");
        let canonical_key = session_key("canonical-interrupted");
        // The retry comes from a later compaction with different text; it
        // must finish the stored episode, not index its own text under the
        // same id.
        let interrupted = "canonical interrupted summary";
        store_interrupted(&owner, &canonical_key, interrupted).await;
        drop(owner);
        let owner = open_owner(temp.path()).await;
        let recall = canonical_recall(temp.path());
        handoff_with_recall(
            &canonical_manager,
            &canonical_key,
            &owner,
            &embedder,
            &recall,
        );
        wait_for_embed_calls(&embedder, 1).await;
        wait_for_store_idle(&owner).await;
        assert_eq!(embedder.texts(), [interrupted]);
        assert_indexes_agree(&owner, &recall, &episode_id(&canonical_key), interrupted).await;

        // Same interruption on a degraded handle: the lock is released and no
        // completion marker exists, so the next degraded handle retries.
        let degraded_manager = manager_with_compaction("degraded-interrupted");
        let degraded_key = session_key("degraded-interrupted");
        store_interrupted(&degraded, &degraded_key, "degraded interrupted summary").await;
        drop(degraded);
        let degraded = Arc::new(EpisodeStore::open_or_degraded(temp.path()).await.unwrap());
        handoff(&degraded_manager, &degraded_key, &degraded, &embedder);
        wait_for_embed_calls(&embedder, 2).await;
        wait_for_store_idle(&degraded).await;
        assert_eq!(embedder.texts()[1], "degraded interrupted summary");

        // Completed retries are idempotent on the next handoff, and after the
        // real redb store is reopened as a new process would.
        handoff(&canonical_manager, &canonical_key, &owner, &embedder);
        handoff(&degraded_manager, &degraded_key, &degraded, &embedder);
        wait_for_store_idle(&owner).await;
        wait_for_store_idle(&degraded).await;
        drop((owner, degraded));
        let owner = open_owner(temp.path()).await;
        handoff(&canonical_manager, &canonical_key, &owner, &embedder);
        wait_for_store_idle(&owner).await;
        assert!(stored(&owner, &episode_id(&canonical_key)).await);
        assert_eq!(calls(&embedder), 2);
    }
}
