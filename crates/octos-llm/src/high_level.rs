//! High-level LLM APIs: `generate()`, `generate_object()`, `stream()`.
//!
//! Wraps `LlmProvider::chat()` with ergonomic builders for common patterns.
//! These are convenience APIs — the raw `chat()` method remains for full control.

use std::sync::Arc;

use eyre::Result;
use octos_core::Message;

use crate::config::{ChatConfig, ResponseFormat};
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// High-level API wrapper around an `LlmProvider`.
pub struct LlmClient {
    provider: Arc<dyn LlmProvider>,
    default_config: ChatConfig,
}

impl LlmClient {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            default_config: ChatConfig::default(),
        }
    }

    pub fn with_config(mut self, config: ChatConfig) -> Self {
        self.default_config = config;
        self
    }

    /// Simple text generation from a prompt string.
    pub async fn generate(&self, prompt: &str) -> Result<String> {
        let messages = vec![Message::user(prompt)];
        let response = self
            .provider
            .chat(&messages, &[], &self.default_config)
            .await?;
        Ok(response.content.unwrap_or_default())
    }

    /// Generate with full message history and tools.
    pub async fn generate_with(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.provider.chat(messages, tools, config).await
    }

    /// Generate a JSON object matching a schema.
    /// Returns the parsed `serde_json::Value`.
    pub async fn generate_object(
        &self,
        prompt: &str,
        schema_name: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut config = self.default_config.clone();
        config.response_format = Some(ResponseFormat::JsonSchema {
            name: schema_name.to_string(),
            schema,
            strict: true,
        });

        let messages = vec![Message::user(prompt)];
        let response = self.provider.chat(&messages, &[], &config).await?;
        let text = response
            .content
            .ok_or_else(|| eyre::eyre!("no content in response"))?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        Ok(value)
    }

    /// Generate a JSON object and deserialize to a typed struct.
    pub async fn generate_typed<T: serde::de::DeserializeOwned>(
        &self,
        prompt: &str,
        schema_name: &str,
        schema: serde_json::Value,
    ) -> Result<T> {
        let value = self.generate_object(prompt, schema_name, schema).await?;
        let typed: T = serde_json::from_value(value)?;
        Ok(typed)
    }

    /// Stream a response from a prompt string.
    pub async fn stream(&self, prompt: &str) -> Result<ChatStream> {
        let messages = vec![Message::user(prompt)];
        self.provider
            .chat_stream(&messages, &[], &self.default_config)
            .await
    }

    /// Stream with full message history and tools.
    pub async fn stream_with(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        self.provider.chat_stream(messages, tools, config).await
    }

    /// Get the underlying provider's model ID.
    pub fn model_id(&self) -> &str {
        self.provider.model_id()
    }

    /// Get the context window size.
    pub fn context_window(&self) -> u32 {
        self.provider.context_window()
    }
}

#[cfg(test)]
mod tests {}
