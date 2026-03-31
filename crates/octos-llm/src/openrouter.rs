//! OpenRouter provider implementation (OpenAI-compatible API).

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{Message, MessageRole};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};

use crate::vision;

use crate::config::ChatConfig;
use crate::openai::parse_openai_sse_events;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, StopReason, TokenUsage, ToolSpec};

/// OpenRouter provider (routes to many LLM providers).
pub struct OpenRouterProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl OpenRouterProvider {
    /// Create a new OpenRouter provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
        }
    }

    /// Create a provider using the OPENROUTER_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .wrap_err("OPENROUTER_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "anthropic/claude-sonnet-4-20250514"))
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| build_api_message(m))
            .collect();

        let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| ApiTool {
                        r#type: "function",
                        function: ApiFunction {
                            name: &t.name,
                            description: &t.description,
                            parameters: &t.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        let request = ApiRequest {
            model: &self.model,
            messages: api_messages,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            tools: api_tools,
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/heyong4725/octos")
            .header("X-Title", "octos")
            .json(&request)
            .send()
            .await
            .wrap_err("failed to send request to OpenRouter")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eyre::bail!(
                "OpenRouter API error: {status} - {}",
                crate::provider::truncate_error_body(&body)
            );
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .wrap_err("failed to parse OpenRouter response")?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("no choices in OpenRouter response"))?;

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| octos_core::ToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments: serde_json::from_str(&tc.function.arguments).unwrap_or_default(),
                metadata: None,
            })
            .collect();

        let stop_reason = match choice.finish_reason.as_str() {
            "stop" => StopReason::EndTurn,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::ContentFiltered,
            _ => StopReason::EndTurn,
        };

        Ok(ChatResponse {
            content: choice.message.content,
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: TokenUsage {
                input_tokens: api_response.usage.prompt_tokens,
                output_tokens: api_response.usage.completion_tokens,
                ..Default::default()
            },
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| build_api_message(m))
            .collect();

        let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| ApiTool {
                        r#type: "function",
                        function: ApiFunction {
                            name: &t.name,
                            description: &t.description,
                            parameters: &t.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        let request = ApiRequest {
            model: &self.model,
            messages: api_messages,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            tools: api_tools,
        };

        let mut body =
            serde_json::to_value(&request).wrap_err("failed to serialize OpenRouter request")?;
        let obj = body
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("failed to build OpenRouter request body"))?;
        obj.insert("stream".into(), true.into());
        obj.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/heyong4725/octos")
            .header("X-Title", "octos")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send streaming request to OpenRouter")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eyre::bail!(
                "OpenRouter API error: {status} - {}",
                crate::provider::truncate_error_body(&text)
            );
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let event_stream =
            sse_stream.flat_map(|event| futures::stream::iter(parse_openai_sse_events(&event)));

        Ok(Box::pin(event_stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "openrouter"
    }
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiTool<'a>>>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ApiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Parts(Vec<ApiContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ApiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ApiImageUrl },
}

#[derive(Serialize)]
struct ApiImageUrl {
    url: String,
}

fn build_api_message(msg: &Message) -> ApiMessage {
    let role = msg.role.as_str();
    let content = build_api_content(msg);
    let tool_calls = msg.tool_calls.as_ref().map(|tcs| {
        tcs.iter()
            .map(|tc| ApiToolCall {
                id: tc.id.clone(),
                function: FunctionCall {
                    name: tc.name.clone(),
                    arguments: tc.arguments.to_string(),
                },
            })
            .collect()
    });
    ApiMessage {
        role,
        content,
        tool_call_id: msg.tool_call_id.as_deref(),
        tool_calls,
    }
}

fn build_api_content(msg: &Message) -> Option<ApiContent> {
    let images: Vec<_> = msg.media.iter().filter(|p| vision::is_image(p)).collect();

    if images.is_empty() {
        if msg.content.is_empty() {
            return match msg.role {
                MessageRole::User => Some(ApiContent::Text("[empty message]".to_string())),
                _ => None,
            };
        }
        return Some(ApiContent::Text(msg.content.clone()));
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(ApiContentPart::ImageUrl {
                image_url: ApiImageUrl {
                    url: format!("data:{mime};base64,{data}"),
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(ApiContentPart::Text {
            text: msg.content.clone(),
        });
    }
    Some(ApiContent::Parts(parts))
}

#[derive(Serialize)]
struct ApiTool<'a> {
    r#type: &'a str,
    function: ApiFunction<'a>,
}

#[derive(Serialize)]
struct ApiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct ApiToolCall {
    id: String,
    function: FunctionCall,
}

#[derive(Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{Message, MessageRole};

    fn text_msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_build_api_content_text_only() {
        let msg = text_msg(MessageRole::User, "hello");
        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Text(t)) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_build_api_content_empty_user_gets_placeholder() {
        let msg = text_msg(MessageRole::User, "");
        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Text(t)) => assert_eq!(t, "[empty message]"),
            other => panic!("expected placeholder Text, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_build_api_content_empty_assistant_returns_none() {
        let msg = text_msg(MessageRole::Assistant, "");
        assert!(build_api_content(&msg).is_none());
    }

    #[test]
    fn test_build_api_content_empty_system_returns_none() {
        let msg = text_msg(MessageRole::System, "");
        assert!(build_api_content(&msg).is_none());
    }

    #[test]
    fn test_build_api_content_with_image() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal valid PNG (1x1 pixel)
        let png_data: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC,
            0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let img_path = dir.path().join("test.png");
        std::fs::write(&img_path, &png_data).unwrap();

        let msg = Message {
            role: MessageRole::User,
            content: "describe this".to_string(),
            media: vec![img_path.to_string_lossy().to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };

        let content = build_api_content(&msg);
        match content {
            Some(ApiContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2); // image + text
                // First part is ImageUrl
                match &parts[0] {
                    ApiContentPart::ImageUrl { image_url } => {
                        assert!(image_url.url.starts_with("data:image/png;base64,"));
                    }
                    _ => panic!("expected ImageUrl first"),
                }
                // Second part is text
                match &parts[1] {
                    ApiContentPart::Text { text } => assert_eq!(text, "describe this"),
                    _ => panic!("expected Text second"),
                }
            }
            other => panic!("expected Parts, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_provider_metadata() {
        let provider = OpenRouterProvider::new("test-key", "test-model");
        assert_eq!(provider.model_id(), "test-model");
        assert_eq!(provider.provider_name(), "openrouter");
    }

    #[test]
    fn test_with_base_url() {
        let provider =
            OpenRouterProvider::new("key", "model").with_base_url("http://localhost:8080");
        assert_eq!(provider.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_api_request_serialization() {
        let msg = ApiMessage {
            role: "user",
            content: Some(ApiContent::Text("hi".to_string())),
            tool_call_id: None,
            tool_calls: None,
        };
        let request = ApiRequest {
            model: "test",
            messages: vec![msg],
            max_tokens: Some(100),
            temperature: None,
            tools: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "test");
        assert_eq!(json["max_tokens"], 100);
        assert!(json.get("temperature").is_none());
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_api_request_with_tools() {
        let schema = serde_json::json!({"type": "object"});
        let tool = ApiTool {
            r#type: "function",
            function: ApiFunction {
                name: "test_fn",
                description: "A test",
                parameters: &schema,
            },
        };
        let request = ApiRequest {
            model: "m",
            messages: vec![],
            max_tokens: None,
            temperature: None,
            tools: Some(vec![tool]),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "test_fn");
    }

    #[test]
    fn test_api_response_deserialization() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.choices[0].finish_reason, "stop");
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 5);
    }

    #[test]
    fn test_api_response_with_tool_calls() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\":\"test\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 15,
                "completion_tokens": 8
            }
        });
        let resp: ApiResponse = serde_json::from_value(json).unwrap();
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_123");
        assert_eq!(tc[0].function.name, "search");
        assert_eq!(tc[0].function.arguments, "{\"query\":\"test\"}");
    }
}
