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
        }
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
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["input"].as_array().unwrap().len(), 2);
    }
}
