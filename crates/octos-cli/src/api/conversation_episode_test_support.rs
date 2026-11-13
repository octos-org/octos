//! Fixtures shared by the conversation-episode tests and both bridges'
//! handoff tests (serve transport, gateway session actor).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use octos_agent::{PromptContextPhase, PromptContextRequest};
use octos_core::{Message, MessageRole, SessionKey};
use octos_llm::EmbeddingProvider;
use octos_memory::{Episode, EpisodeSource, EpisodeStore};

use super::{BridgeMemoryContext, conversation_episode_id};

/// An embedder that counts its calls.
pub(crate) struct CountingEmbedder {
    pub(crate) calls: AtomicUsize,
}

impl CountingEmbedder {
    pub(crate) fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
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

/// Twelve alternating user/assistant turns about release planning.
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

/// A bridge's memory wiring: profile `main` in `workspace`, memory on, the
/// counting embedder, no recall.
pub(crate) fn bridge_memory(
    store: &Arc<EpisodeStore>,
    embedder: &Arc<CountingEmbedder>,
    workspace: &Path,
) -> BridgeMemoryContext {
    BridgeMemoryContext {
        profile_id: "main".to_owned(),
        workspace_root: workspace.to_path_buf(),
        memory: store.clone(),
        memory_enabled: true,
        embedder: Some(embedder.clone() as Arc<dyn EmbeddingProvider>),
        recall: None,
    }
}

/// A turn-start request with a one-token window, so any history compacts.
pub(crate) fn compacting_turn_start() -> PromptContextRequest {
    PromptContextRequest {
        phase: PromptContextPhase::TurnStart,
        iteration: 0,
        provider_name: "test".to_owned(),
        model_id: "test".to_owned(),
        context_window: 1,
    }
}

/// Waits until `session`'s Conversation episode under [`bridge_memory`] is
/// stored and embedded once.
pub(crate) async fn assert_bridge_handed_off(
    store: &Arc<EpisodeStore>,
    embedder: &CountingEmbedder,
    session: &SessionKey,
    workspace: &Path,
) {
    let episode_id = conversation_episode_id("main", session, workspace);
    let episode = wait_for_episode(store, &episode_id).await;
    wait_for_embed_calls(embedder, 1).await;
    assert_eq!(episode.source, EpisodeSource::Conversation);
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
