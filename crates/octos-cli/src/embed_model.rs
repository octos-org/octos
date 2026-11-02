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

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::EmbeddingProvider as _;

    struct DeferredPriorityRecorder(std::sync::Mutex<Option<octos_llm::EmbeddingPriority>>);

    #[async_trait::async_trait]
    impl octos_llm::EmbeddingProvider for DeferredPriorityRecorder {
        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            *self.0.lock().unwrap() = Some(octos_llm::EmbeddingPriority::Foreground);
            Ok(texts.iter().map(|_| vec![0.0; 768]).collect())
        }

        async fn embed_with_priority(
            &self,
            texts: &[&str],
            priority: octos_llm::EmbeddingPriority,
        ) -> Result<Vec<Vec<f32>>> {
            *self.0.lock().unwrap() = Some(priority);
            Ok(texts.iter().map(|_| vec![0.0; 768]).collect())
        }

        fn dimension(&self) -> usize {
            768
        }
    }

    #[tokio::test]
    async fn deferred_provider_forwards_background_priority() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        let file = std::fs::File::create(default_model_path(dir.path())).unwrap();
        file.set_len(DEFAULT_MODEL_BYTES).unwrap();
        let provider = DeferredEmbeddingProvider::new(dir.path().to_path_buf());
        let recorder = std::sync::Arc::new(DeferredPriorityRecorder(std::sync::Mutex::new(None)));
        let as_provider: std::sync::Arc<dyn octos_llm::EmbeddingProvider> = recorder.clone();
        assert!(provider.inner.set(as_provider).is_ok());

        provider
            .embed_with_priority(&["episode"], octos_llm::EmbeddingPriority::Background)
            .await
            .unwrap();
        assert_eq!(
            *recorder.0.lock().unwrap(),
            Some(octos_llm::EmbeddingPriority::Background)
        );
    }

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
        // Lazy construction defers malformed-GGUF detection to the first
        // blocking load/decode rather than the factory call.
        assert!(!error.to_string().is_empty());
    }
}
