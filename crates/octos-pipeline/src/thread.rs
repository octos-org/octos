//! Thread resolution for LLM session reuse across pipeline nodes.
//!
//! Allows nodes to share conversation history by referencing a thread ID.
//! Nodes with the same `thread` attribute reuse the same message history,
//! enabling multi-turn conversations across pipeline steps.
//!
//! TODO: Wire into executor to resolve thread references during node execution.

use std::collections::HashMap;
use std::sync::Arc;

use octos_core::Message;
use tokio::sync::RwLock;

/// Maximum number of threads per registry.
const MAX_THREADS: usize = 1000;

/// Maximum number of messages per thread.
const MAX_MESSAGES_PER_THREAD: usize = 10_000;

/// A conversation thread shared between pipeline nodes.
#[derive(Debug, Clone)]
pub struct Thread {
    /// Thread identifier.
    pub id: String,
    /// Accumulated messages in this thread.
    pub messages: Vec<Message>,
}

impl Thread {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            messages: Vec::new(),
        }
    }

    /// Append a message to the thread.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Number of messages in the thread.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Registry of threads for a pipeline run.
///
/// Thread-safe: multiple nodes can read/write concurrently.
#[derive(Clone)]
pub struct ThreadRegistry {
    threads: Arc<RwLock<HashMap<String, Thread>>>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self {
            threads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a thread by ID, returning its current messages.
    pub async fn get_messages(&self, thread_id: &str) -> Vec<Message> {
        let threads = self.threads.read().await;
        threads
            .get(thread_id)
            .map(|t| t.messages.clone())
            .unwrap_or_default()
    }

    /// Append messages to a thread (creates if not exists).
    ///
    /// Returns an error if the thread or message count limits are exceeded.
    pub async fn append(&self, thread_id: &str, messages: Vec<Message>) -> eyre::Result<()> {
        let mut threads = self.threads.write().await;
        if !threads.contains_key(thread_id) && threads.len() >= MAX_THREADS {
            eyre::bail!("thread limit exceeded ({MAX_THREADS})");
        }
        let thread = threads
            .entry(thread_id.to_string())
            .or_insert_with(|| Thread::new(thread_id));
        let remaining = MAX_MESSAGES_PER_THREAD.saturating_sub(thread.len());
        if messages.len() > remaining {
            eyre::bail!(
                "message limit exceeded for thread '{}' ({MAX_MESSAGES_PER_THREAD})",
                thread_id
            );
        }
        for msg in messages {
            thread.push(msg);
        }
        Ok(())
    }

    /// Get thread IDs.
    pub async fn thread_ids(&self) -> Vec<String> {
        let threads = self.threads.read().await;
        threads.keys().cloned().collect()
    }

    /// Check if a thread exists.
    pub async fn contains(&self, thread_id: &str) -> bool {
        let threads = self.threads.read().await;
        threads.contains_key(thread_id)
    }

    /// Resolve a thread reference from a node attribute.
    ///
    /// If the attribute is "new", returns None (fresh thread).
    /// Otherwise returns the thread ID to reuse.
    pub fn resolve_thread_attr(attr: &str) -> Option<String> {
        let attr = attr.trim();
        if attr.is_empty() || attr == "new" {
            None
        } else {
            Some(attr.to_string())
        }
    }
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::Message;

    #[test]
    fn should_create_empty_thread() {
        let thread = Thread::new("t1");
        assert_eq!(thread.id, "t1");
        assert!(thread.is_empty());
    }

    #[test]
    fn should_push_messages() {
        let mut thread = Thread::new("t1");
        thread.push(Message::user("hello"));
        thread.push(Message::assistant("hi"));
        assert_eq!(thread.len(), 2);
    }

    #[tokio::test]
    async fn should_get_empty_for_unknown_thread() {
        let registry = ThreadRegistry::new();
        let msgs = registry.get_messages("nonexistent").await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn should_append_and_retrieve() {
        let registry = ThreadRegistry::new();
        registry
            .append("t1", vec![Message::user("hello")])
            .await
            .unwrap();
        registry
            .append("t1", vec![Message::assistant("hi")])
            .await
            .unwrap();

        let msgs = registry.get_messages("t1").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn should_track_thread_ids() {
        let registry = ThreadRegistry::new();
        registry
            .append("t1", vec![Message::user("a")])
            .await
            .unwrap();
        registry
            .append("t2", vec![Message::user("b")])
            .await
            .unwrap();

        let ids = registry.thread_ids().await;
        assert_eq!(ids.len(), 2);
        assert!(registry.contains("t1").await);
        assert!(registry.contains("t2").await);
        assert!(!registry.contains("t3").await);
    }

    #[test]
    fn should_resolve_thread_attr() {
        assert_eq!(ThreadRegistry::resolve_thread_attr("new"), None);
        assert_eq!(ThreadRegistry::resolve_thread_attr(""), None);
        assert_eq!(
            ThreadRegistry::resolve_thread_attr("shared_ctx"),
            Some("shared_ctx".to_string())
        );
    }
}
