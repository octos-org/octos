//! Provisioning of the default in-process embedding model.
//!
//! Model provisioning lives in `octos-services`; the deferred provider stays
//! here because initializing the ready model uses the CLI's embedder factory.

use std::path::PathBuf;

use eyre::{Result, bail};

pub use octos_services::embed_model::*;

/// A keyword-usable placeholder that becomes a real llama.cpp provider only
/// after the model exists. Its `embed` errors are intentionally retryable by
/// the recall backfill loop, never a reason to block session startup.
pub struct DeferredEmbeddingProvider {
    data_dir: PathBuf,
    inner: std::sync::OnceLock<std::sync::Arc<dyn octos_llm::EmbeddingProvider>>,
}

impl DeferredEmbeddingProvider {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            inner: std::sync::OnceLock::new(),
        }
    }
}

#[async_trait::async_trait]
impl octos_llm::EmbeddingProvider for DeferredEmbeddingProvider {
    fn dimension(&self) -> usize {
        768
    }

    fn is_ready(&self) -> bool {
        model_status(&self.data_dir).complete
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_with_priority(texts, octos_llm::EmbeddingPriority::Foreground)
            .await
    }

    async fn embed_with_priority(
        &self,
        texts: &[&str],
        priority: octos_llm::EmbeddingPriority,
    ) -> Result<Vec<Vec<f32>>> {
        if !self.is_ready() {
            bail!("default embedding model is still downloading");
        }
        if self.inner.get().is_none() {
            let provider = crate::commands::chat::create_embedder_in(
                &crate::config::Config::default(),
                &self.data_dir,
            )
            .ok_or_else(|| eyre::eyre!("default embedding model failed to initialize"))?;
            let _ = self.inner.set(provider);
        }
        let provider = self
            .inner
            .get()
            .ok_or_else(|| eyre::eyre!("default embedding model failed to initialize"))?;
        provider.embed_with_priority(texts, priority).await
    }
}
