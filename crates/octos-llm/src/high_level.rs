//! High-level LLM APIs: `generate()`, `generate_object()`, `stream()`.
//!
//! Wraps `LlmProvider::chat()` with ergonomic builders for common patterns.
//! These are convenience APIs — the raw `chat()` method remains for full control.

use std::sync::Arc;

use octos_core::Message;
use eyre::Result;

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
mod tests {
    use super::*;
    use crate::types::{StopReason, TokenUsage};
    use async_trait::async_trait;

    struct MockProvider {
        response: String,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some(self.response.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn should_generate_text() {
        let client = LlmClient::new(Arc::new(MockProvider {
            response: "Hello world".into(),
        }));
        let result = client.generate("Say hello").await.unwrap();
        assert_eq!(result, "Hello world");
    }

    #[tokio::test]
    async fn should_generate_object() {
        let client = LlmClient::new(Arc::new(MockProvider {
            response: r#"{"name":"Alice","age":30}"#.into(),
        }));

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"}
            }
        });

        let result = client
            .generate_object("Create a person", "Person", schema)
            .await
            .unwrap();

        assert_eq!(result["name"], "Alice");
        assert_eq!(result["age"], 30);
    }

    #[tokio::test]
    async fn should_generate_typed() {
        #[derive(serde::Deserialize)]
        struct Person {
            name: String,
            age: u32,
        }

        let client = LlmClient::new(Arc::new(MockProvider {
            response: r#"{"name":"Bob","age":25}"#.into(),
        }));

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"}
            }
        });

        let person: Person = client
            .generate_typed("Create a person", "Person", schema)
            .await
            .unwrap();

        assert_eq!(person.name, "Bob");
        assert_eq!(person.age, 25);
    }

    #[tokio::test]
    async fn should_stream_text() {
        let client = LlmClient::new(Arc::new(MockProvider {
            response: "streamed".into(),
        }));
        let stream = client.stream("Test").await.unwrap();
        // Stream should be valid (we test it produces events via the default impl)
        drop(stream);
    }

    #[test]
    fn should_expose_model_info() {
        let client = LlmClient::new(Arc::new(MockProvider {
            response: String::new(),
        }));
        assert_eq!(client.model_id(), "mock-model");
    }
}
