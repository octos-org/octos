//! Configuration for chat requests.

use serde::{Deserialize, Serialize};

/// Configuration for a chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    /// Maximum tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Temperature for sampling (0.0 = deterministic, 1.0 = creative).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// How the model should choose tools.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Reasoning effort for thinking models (none/low/medium/high/max).
    /// Maps to provider-specific parameters (OpenAI reasoning.effort,
    /// Anthropic thinking budget, Gemini thinkingConfig).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Structured output format. When set, the model will return responses
    /// conforming to the given schema (JSON mode or JSON Schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Anthropic-only: opaque `context_management` payload (e.g. the
    /// `clear_tool_uses_20250919` config) that decorates the request so the
    /// server performs tier-2 tool-use clearing on its side. M8.5 wires this
    /// from the `ApiMicroCompactionConfig` builder. Providers other than
    /// Anthropic ignore this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    /// Extra sampler params for OpenAI-compatible servers, flattened verbatim
    /// into the request body — e.g. `{"repeat_penalty": 1.1, "top_p": 0.95}`.
    /// For params octos does not model (`repeat_penalty`, `top_p`, `top_k`,
    /// `min_p`, `frequency_penalty`, `presence_penalty`, …). `None` → nothing is
    /// added, so cloud requests are unchanged. Do not put `temperature` /
    /// `max_tokens` here — use their dedicated fields. See issue #2172.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    /// Per-request prompt-cache retention preference.
    ///
    /// [`CacheRetention::None`] asks the provider to skip prompt-cache
    /// WRITES for this one request — on Anthropic that means emitting NO
    /// `cache_control` breakpoints, since every breakpoint both reads and
    /// writes and an unread write still bills the 1.25x premium. Set it on
    /// one-shot calls (compaction summaries, sub-agent digests) whose
    /// prefix is never sent again; the agent loop, whose prefix IS replayed
    /// every iteration, must stay on [`CacheRetention::Default`]. Mirrors
    /// pi's `cacheRetention: "none"` on its summarization requests.
    /// Providers without explicit cache breakpoints ignore this field.
    #[serde(default, skip_serializing_if = "CacheRetention::is_default")]
    pub cache_retention: CacheRetention,
}

/// Prompt-cache retention for a single request. See
/// [`ChatConfig::cache_retention`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// The provider's configured caching behavior (on Anthropic: the three
    /// ephemeral `cache_control` breakpoints, unless the provider was built
    /// with caching disabled).
    #[default]
    Default,
    /// Do not write this request's prefix to the provider's prompt cache.
    None,
}

impl CacheRetention {
    /// `skip_serializing_if` hook: an unset preference stays off the wire so
    /// persisted `ChatConfig` JSON keeps its pre-field shape.
    pub fn is_default(&self) -> bool {
        matches!(self, CacheRetention::Default)
    }
}

/// Structured output format for chat responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text (default behavior).
    Text,
    /// JSON mode — model returns valid JSON but without schema enforcement.
    JsonObject,
    /// JSON Schema mode — model returns JSON conforming to the provided schema.
    JsonSchema {
        /// Schema name (required by OpenAI).
        name: String,
        /// JSON Schema the response must conform to.
        schema: serde_json::Value,
        /// Whether to enforce strict schema adherence (default: true).
        #[serde(default = "default_strict")]
        strict: bool,
    },
}

fn default_strict() -> bool {
    true
}

/// Reasoning effort level for thinking/reasoning models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Disable reasoning where the provider supports it.
    #[serde(rename = "none")]
    Disabled,
    Low,
    Medium,
    High,
    /// Maximum reasoning. DeepSeek V4 accepts `reasoning_effort:"max"`; providers
    /// without a distinct max tier (OpenAI/Grok) clamp this to `high`, and Gemini
    /// maps it to an unbounded thinking budget.
    Max,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            max_tokens: Some(crate::context::default_max_tokens()),
            temperature: Some(0.0),
            tool_choice: ToolChoice::Auto,
            stop_sequences: Vec::new(),
            reasoning_effort: None,
            response_format: None,
            context_management: None,
            sampling_params: None,
            cache_retention: CacheRetention::Default,
        }
    }
}

/// How the model should choose tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to use tools.
    #[default]
    Auto,
    /// Model must use a tool.
    Required,
    /// Model must not use tools.
    None,
    /// Model must use a specific tool.
    Specific { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_deserialize_disabled_reasoning_effort_from_none() {
        let effort: ReasoningEffort = serde_json::from_value(serde_json::json!("none"))
            .expect("none should disable reasoning");
        assert_eq!(
            serde_json::to_value(effort).unwrap(),
            serde_json::json!("none")
        );
    }

    #[test]
    fn test_chat_config_defaults() {
        let config = ChatConfig::default();
        assert_eq!(
            config.max_tokens,
            Some(crate::context::default_max_tokens())
        );
        assert_eq!(config.temperature, Some(0.0));
        assert!(matches!(config.tool_choice, ToolChoice::Auto));
        assert!(config.stop_sequences.is_empty());
    }

    #[test]
    fn test_tool_choice_default_is_auto() {
        let choice = ToolChoice::default();
        assert!(matches!(choice, ToolChoice::Auto));
    }

    #[test]
    fn should_default_cache_retention_to_provider_default_when_unset() {
        assert_eq!(
            ChatConfig::default().cache_retention,
            CacheRetention::Default
        );
    }

    #[test]
    fn should_keep_cache_retention_off_the_wire_when_default() {
        // Persisted ChatConfig JSON must keep its pre-field byte shape for a
        // config that never touches the preference.
        let json = serde_json::to_value(ChatConfig::default()).unwrap();
        assert!(json.get("cache_retention").is_none());
    }

    #[test]
    fn should_round_trip_cache_retention_none_as_snake_case() {
        let config = ChatConfig {
            cache_retention: CacheRetention::None,
            ..Default::default()
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["cache_retention"], "none");
        let decoded: ChatConfig = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.cache_retention, CacheRetention::None);
    }

    #[test]
    fn test_chat_config_serde_roundtrip() {
        let config = ChatConfig {
            max_tokens: Some(2048),
            temperature: Some(0.7),
            tool_choice: ToolChoice::Required,
            stop_sequences: vec!["STOP".to_string()],
            reasoning_effort: None,
            response_format: None,
            context_management: None,
            sampling_params: None,
            cache_retention: CacheRetention::Default,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ChatConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.max_tokens, Some(2048));
        assert_eq!(deserialized.temperature, Some(0.7));
        assert!(matches!(deserialized.tool_choice, ToolChoice::Required));
        assert_eq!(deserialized.stop_sequences, vec!["STOP"]);
    }

    #[test]
    fn test_chat_config_skip_serializing_none() {
        let config = ChatConfig {
            max_tokens: None,
            temperature: None,
            tool_choice: ToolChoice::Auto,
            stop_sequences: vec![],
            reasoning_effort: None,
            response_format: None,
            context_management: None,
            sampling_params: None,
            cache_retention: CacheRetention::Default,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("stop_sequences").is_none());
        assert!(json.get("context_management").is_none());
    }

    #[test]
    fn should_serialize_context_management_when_present() {
        let config = ChatConfig {
            context_management: Some(serde_json::json!({
                "edits": [
                    { "type": "clear_tool_uses_20250919", "keep": { "type": "input_tokens", "value": 10 } }
                ]
            })),
            ..Default::default()
        };
        let json = serde_json::to_value(&config).unwrap();
        let cm = json.get("context_management").expect("field present");
        assert!(cm.is_object());
        assert!(cm.get("edits").is_some());
    }

    #[test]
    fn test_tool_choice_specific_serde() {
        let choice = ToolChoice::Specific {
            name: "search".to_string(),
        };
        let json = serde_json::to_value(&choice).unwrap();
        // Externally tagged enum: {"specific": {"name": "search"}}
        assert_eq!(json["specific"]["name"], "search");
        let deserialized: ToolChoice = serde_json::from_value(json).unwrap();
        match deserialized {
            ToolChoice::Specific { name } => assert_eq!(name, "search"),
            _ => panic!("expected Specific"),
        }
    }

    #[test]
    fn test_tool_choice_none_serde() {
        let choice = ToolChoice::None;
        let json = serde_json::to_value(&choice).unwrap();
        let deserialized: ToolChoice = serde_json::from_value(json).unwrap();
        assert!(matches!(deserialized, ToolChoice::None));
    }

    #[test]
    fn test_response_format_json_object_serde() {
        let rf = ResponseFormat::JsonObject;
        let json = serde_json::to_value(&rf).unwrap();
        assert_eq!(json["type"], "json_object");
        let deserialized: ResponseFormat = serde_json::from_value(json).unwrap();
        assert!(matches!(deserialized, ResponseFormat::JsonObject));
    }

    #[test]
    fn test_response_format_json_schema_serde() {
        let rf = ResponseFormat::JsonSchema {
            name: "person".into(),
            schema: serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            strict: true,
        };
        let json = serde_json::to_value(&rf).unwrap();
        assert_eq!(json["type"], "json_schema");
        assert_eq!(json["name"], "person");
        assert!(json["strict"].as_bool().unwrap());

        let deserialized: ResponseFormat = serde_json::from_value(json).unwrap();
        match deserialized {
            ResponseFormat::JsonSchema { name, strict, .. } => {
                assert_eq!(name, "person");
                assert!(strict);
            }
            _ => panic!("expected JsonSchema"),
        }
    }

    #[test]
    fn test_response_format_skipped_when_none() {
        let config = ChatConfig::default();
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("response_format").is_none());
    }
}
