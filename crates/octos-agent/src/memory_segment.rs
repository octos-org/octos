//! Memory prompt-segment provider: keeps the injected memory block fresh.
//!
//! Registered on conversation agents when `memory.refresh.enabled` is on.
//! [`crate::Agent::refresh_prompt_segments`] calls [`MemorySegmentProvider::refresh`]
//! at every turn start; the provider re-renders the block only when
//! `MEMORY.md` changed on disk (mtime+len) or the local date rolled over
//! (daily-note windows shift) — the unchanged path is a single `stat`.

use std::sync::Arc;
use std::time::SystemTime;

use octos_memory::MemoryStore;

use crate::agent::PromptSegmentProvider;

/// Name of the named prompt segment carrying the memory block.
pub const MEMORY_SEGMENT_NAME: &str = "memory";

/// Capture-policy block appended to the memory segment when the
/// memory-refresh feature is enabled. Shared by the chat path (via
/// [`MemorySegmentProvider`]) and the gateway prompt builder so both
/// surfaces teach the same rules.
pub const MEMORY_CAPTURE_POLICY: &str = "## Memory Capture\n\
Your Long-term Memory above may be stale — prefer fresh evidence from this \
conversation when they conflict, and say so when a consequential answer relies \
on possibly-stale memory. Capture durable observations with the `memory_note` \
tool (notes are consolidated later; MOST TURNS NEED NO NOTE):\n\
- The user explicitly asks to remember/forget/update something -> \
memory_note(kind=\"user_request\") quoting their request.\n\
- This conversation contradicts a Long-term Memory entry -> \
memory_note(kind=\"correction\"), with replaces_id set to the entry's id when one is shown.\n\
- You learned a durable preference, workflow, or environment fact -> \
memory_note(kind=\"fact\") — only if a future conversation would plausibly go \
better for knowing it.\n\
Never edit files under the memory directory with file tools; `memory_note` is \
the only memory write path.";

/// Change fingerprint for the memory block inputs.
#[derive(PartialEq, Eq, Clone)]
struct Fingerprint {
    /// (mtime, len) of MEMORY.md; `None` when the file doesn't exist.
    memory_md: Option<(SystemTime, u64)>,
    /// Local date — daily-note windows shift at midnight.
    date: String,
}

/// [`PromptSegmentProvider`] for the `"memory"` segment.
pub struct MemorySegmentProvider {
    store: Arc<MemoryStore>,
    max_inject_tokens: usize,
    include_capture_policy: bool,
    last: tokio::sync::Mutex<Option<Fingerprint>>,
}

impl MemorySegmentProvider {
    pub fn new(
        store: Arc<MemoryStore>,
        max_inject_tokens: usize,
        include_capture_policy: bool,
    ) -> Self {
        Self {
            store,
            max_inject_tokens,
            include_capture_policy,
            last: tokio::sync::Mutex::new(None),
        }
    }

    async fn fingerprint(&self) -> Fingerprint {
        let memory_md = tokio::fs::metadata(self.store.memory_md_path())
            .await
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        Fingerprint {
            memory_md,
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        }
    }

    /// Render the full segment content (memory block + optional policy).
    pub async fn render(&self) -> String {
        let memory_ctx = self
            .store
            .get_injectable_context(self.max_inject_tokens)
            .await;
        compose_memory_segment(&memory_ctx, self.include_capture_policy)
    }
}

/// Compose the memory segment from the rendered block + optional capture
/// policy. Shared with the gateway prompt builder so both paths emit the
/// same bytes.
pub fn compose_memory_segment(memory_ctx: &str, include_capture_policy: bool) -> String {
    match (memory_ctx.is_empty(), include_capture_policy) {
        (true, false) => String::new(),
        (true, true) => MEMORY_CAPTURE_POLICY.to_string(),
        (false, false) => memory_ctx.to_string(),
        (false, true) => format!("{memory_ctx}\n\n{MEMORY_CAPTURE_POLICY}"),
    }
}

#[async_trait::async_trait]
impl PromptSegmentProvider for MemorySegmentProvider {
    fn segment_name(&self) -> &str {
        MEMORY_SEGMENT_NAME
    }

    async fn refresh(&self) -> Option<String> {
        let current = self.fingerprint().await;
        {
            let mut last = self.last.lock().await;
            if last.as_ref() == Some(&current) {
                return None;
            }
            *last = Some(current);
        }
        Some(self.render().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_render_on_first_refresh_then_stat_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        store.write_long_term("a durable fact").await.unwrap();

        let provider = MemorySegmentProvider::new(store, 2500, false);
        let first = provider.refresh().await;
        assert!(first.is_some_and(|c| c.contains("a durable fact")));
        // Unchanged file → no re-render.
        assert!(provider.refresh().await.is_none());
    }

    #[tokio::test]
    async fn should_re_render_when_memory_md_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        store.write_long_term("version one!").await.unwrap();

        let provider = MemorySegmentProvider::new(store.clone(), 2500, false);
        assert!(provider.refresh().await.is_some());
        assert!(provider.refresh().await.is_none());

        store
            .write_long_term("version two, longer content")
            .await
            .unwrap();
        let refreshed = provider.refresh().await;
        assert!(refreshed.is_some_and(|c| c.contains("version two")));
    }

    #[tokio::test]
    async fn should_include_policy_when_capture_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());

        // Empty memory + capture on → policy alone.
        let provider = MemorySegmentProvider::new(store.clone(), 2500, true);
        let content = provider.refresh().await.expect("first refresh renders");
        assert!(content.contains("## Memory Capture"));
        assert!(content.contains("memory_note"));

        // Empty memory + capture off → empty segment.
        let provider_off = MemorySegmentProvider::new(store, 2500, false);
        assert_eq!(provider_off.refresh().await, Some(String::new()));
    }

    #[test]
    fn should_compose_segment_in_all_four_shapes() {
        assert_eq!(compose_memory_segment("", false), "");
        assert_eq!(compose_memory_segment("", true), MEMORY_CAPTURE_POLICY);
        assert_eq!(compose_memory_segment("mem", false), "mem");
        let both = compose_memory_segment("mem", true);
        assert!(both.starts_with("mem\n\n## Memory Capture"));
    }
}
