//! Embedding provider trait and OpenAI implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::provider::truncate_error_body;

/// Trait for embedding providers.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Generate embeddings for a batch of texts.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// The dimensionality of the embedding vectors.
    fn dimension(&self) -> usize;
}

/// OpenAI-compatible embedding provider.
pub struct OpenAIEmbedder {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
    dimensions: Option<u32>,
}

impl OpenAIEmbedder {
    /// Create a new OpenAI embedder with the given API key.
    /// Default model: text-embedding-3-small (1536 dimensions).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_EMBEDDING_TIMEOUT_SECS,
                crate::provider::DEFAULT_EMBEDDING_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: "text-embedding-3-small".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            dimensions: None,
        }
    }

    /// Request a specific output dimension (OpenAI-standard `dimensions`
    /// field). The episodic HNSW index is built at a fixed dimension
    /// (1536 by default) — set this when the model's native size differs
    /// (e.g. DashScope text-embedding-v4 defaults to 1024) or mismatched
    /// vectors are dropped to BM25-only.
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set a custom model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    /// OpenAI-standard optional output dimension (supported by
    /// text-embedding-3-* and OpenAI-compatible providers like DashScope
    /// text-embedding-v4). Skipped when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for OpenAIEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = EmbeddingRequest {
            model: &self.model,
            input: texts,
            dimensions: self.dimensions,
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.api_key.expose_secret())
            .json(&body)
            .send()
            .await
            .wrap_err("embedding request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!(
                "embedding API error ({}): {}",
                status,
                truncate_error_body(&body)
            );
        }

        let result: EmbeddingResponse = resp
            .json()
            .await
            .wrap_err("failed to parse embedding response")?;

        Ok(result.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        // An explicit requested dimension IS the output size (the API
        // truncates/projects to it) — the model-default table only applies
        // when unset. Consumers size indexes from this value.
        if let Some(d) = self.dimensions {
            return d as usize;
        }
        match self.model.as_str() {
            "text-embedding-3-large" => 3072,
            _ => 1536, // text-embedding-3-small, ada-002
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_request_serialization() {
        let texts = ["hello world", "foo bar"];
        let req = EmbeddingRequest {
            model: "text-embedding-3-small",
            input: &texts,
            dimensions: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn request_serializes_dimensions_only_when_set() {
        let without = EmbeddingRequest {
            model: "text-embedding-3-small",
            input: &["x"],
            dimensions: None,
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(
            !json.contains("dimensions"),
            "unset must be skipped: {json}"
        );

        let with = EmbeddingRequest {
            model: "text-embedding-v4",
            input: &["x"],
            dimensions: Some(1536),
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("\"dimensions\":1536"), "{json}");
    }

    #[test]
    fn dimension_reflects_requested_dimensions() {
        let default = OpenAIEmbedder::new("k");
        assert_eq!(EmbeddingProvider::dimension(&default), 1536);
        let large = OpenAIEmbedder::new("k").with_model("text-embedding-3-large");
        assert_eq!(EmbeddingProvider::dimension(&large), 3072);
        // Requested size wins over the model default table.
        let pinned = OpenAIEmbedder::new("k")
            .with_model("text-embedding-3-large")
            .with_dimensions(1536);
        assert_eq!(EmbeddingProvider::dimension(&pinned), 1536);
    }

    #[test]
    fn builder_overrides_model_and_dimensions() {
        let e = OpenAIEmbedder::new("k")
            .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1")
            .with_model("text-embedding-v4")
            .with_dimensions(1536);
        assert_eq!(e.model, "text-embedding-v4");
        assert_eq!(e.dimensions, Some(1536));
        assert!(e.base_url.contains("compatible-mode"));
    }
}
