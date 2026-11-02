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
        provider.embed(texts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::EmbeddingProvider as _;

    #[tokio::test]
    async fn deferred_provider_keeps_recall_keyword_only_until_model_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DeferredEmbeddingProvider::new(dir.path().to_path_buf());
        assert!(!provider.is_ready());
        assert!(provider.embed(&["keyword-only"]).await.is_err());
        let config = crate::runtime::profile::recall_config_for(
            &crate::config::Config::default(),
            Some(&provider),
        );
        assert!(config.embedder_id.is_empty());

        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        let file = std::fs::File::create(default_model_path(dir.path())).unwrap();
        file.set_len(DEFAULT_MODEL_BYTES).unwrap();
        assert!(provider.is_ready());
        let config = crate::runtime::profile::recall_config_for(
            &crate::config::Config::default(),
            Some(&provider),
        );
        assert_eq!(config.embedder_id, DEFAULT_MODEL_ID);
        let error = provider
            .embed(&["invalid model bytes"])
            .await
            .expect_err("an invalid same-length model must return an error, not panic");
        assert!(error.to_string().contains("failed to initialize"));
    }
}
