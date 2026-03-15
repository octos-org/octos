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
    /// Reasoning effort for thinking models (low/medium/high).
    /// Maps to provider-specific parameters (OpenAI reasoning.effort,
    /// Anthropic thinking budget, Gemini thinkingConfig).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Structured output format. When set, the model will return responses
    /// conforming to the given schema (JSON mode or JSON Schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
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
    Low,
    Medium,
    High,
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
    fn test_chat_config_defaults() {
        let config = ChatConfig::default();
        assert_eq!(config.max_tokens, Some(crate::context::default_max_tokens()));
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
    fn test_chat_config_serde_roundtrip() {
        let config = ChatConfig {
            max_tokens: Some(2048),
            temperature: Some(0.7),
            tool_choice: ToolChoice::Required,
            stop_sequences: vec!["STOP".to_string()],
            reasoning_effort: None,
            response_format: None,
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
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("stop_sequences").is_none());
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
