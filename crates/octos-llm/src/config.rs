//! Configuration for chat requests.

use serde::{Deserialize, Serialize};

/// Provider-neutral prompt-cache metadata attached by the agent immediately
/// before dispatch. Providers may consume only the fields they explicitly
/// support; compatible/unknown endpoints must omit provider-specific wire
/// fields entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheContext {
    /// Stable, privacy-preserving traffic-affinity key (max 64 chars).
    pub affinity_key: String,
    /// Identity of the current cache-relevant System/tool/model epoch.
    pub epoch_id: String,
    pub stable_prefix_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_boundaries: Vec<SemanticCheckpointHint>,
}

/// Optional semantic boundary offered to local/hybrid runtimes. It is a hint,
/// never proof that a checkpoint exists and never required for correctness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticCheckpointHint {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub prefix_hash: String,
    pub prefix_token_estimate: usize,
    pub estimated_recompute_tokens: usize,
    pub checkpoint_priority: u8,
}

impl PromptCacheContext {
    /// Deepest semantic checkpoint whose exact prefix survives in both
    /// contexts. A local runtime can use this to avoid restoring state past an
    /// edited tool/thinking boundary.
    pub fn deepest_shared_checkpoint<'a>(
        &'a self,
        next: &Self,
    ) -> Option<&'a SemanticCheckpointHint> {
        self.semantic_boundaries
            .iter()
            .zip(next.semantic_boundaries.iter())
            .take_while(|(previous, current)| {
                previous.boundary_kind == current.boundary_kind
                    && previous.prefix_hash == current.prefix_hash
            })
            .map(|(previous, _)| previous)
            .last()
    }
}

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
    /// Internal prompt-cache affinity and semantic checkpoint metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_context: Option<PromptCacheContext>,
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
            prompt_cache_context: None,
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

impl ToolChoice {
    /// Whether this choice needs an explicit wire field. `Auto` is every
    /// provider's default, so it is omitted and request bodies built before
    /// `tool_choice` was serialized stay byte-identical (cache-relevant on
    /// providers that key on the whole request).
    fn is_explicit(&self, has_tools: bool) -> bool {
        has_tools && !matches!(self, Self::Auto)
    }

    /// OpenAI chat-completions / OpenRouter form: `"none" | "required" |
    /// {"type":"function","function":{"name":…}}`.
    pub fn openai_chat_wire(&self, has_tools: bool) -> Option<serde_json::Value> {
        if !self.is_explicit(has_tools) {
            return Option::None;
        }
        Some(match self {
            Self::Auto => serde_json::Value::String("auto".into()),
            Self::Required => serde_json::Value::String("required".into()),
            Self::None => serde_json::Value::String("none".into()),
            Self::Specific { name } => serde_json::json!({
                "type": "function",
                "function": { "name": name },
            }),
        })
    }

    /// OpenAI Responses form: `"none" | "required" |
    /// {"type":"function","name":…}`.
    pub fn openai_responses_wire(&self, has_tools: bool) -> Option<serde_json::Value> {
        if !self.is_explicit(has_tools) {
            return Option::None;
        }
        Some(match self {
            Self::Auto => serde_json::Value::String("auto".into()),
            Self::Required => serde_json::Value::String("required".into()),
            Self::None => serde_json::Value::String("none".into()),
            Self::Specific { name } => serde_json::json!({
                "type": "function",
                "name": name,
            }),
        })
    }

    /// Anthropic Messages form: `{"type":"none"|"auto"|"any"|"tool","name":…}`.
    pub fn anthropic_wire(&self, has_tools: bool) -> Option<serde_json::Value> {
        if !self.is_explicit(has_tools) {
            return Option::None;
        }
        Some(match self {
            Self::Auto => serde_json::json!({ "type": "auto" }),
            Self::Required => serde_json::json!({ "type": "any" }),
            Self::None => serde_json::json!({ "type": "none" }),
            Self::Specific { name } => serde_json::json!({ "type": "tool", "name": name }),
        })
    }

    /// Gemini `toolConfig.functionCallingConfig` form.
    pub fn gemini_function_calling_config(&self, has_tools: bool) -> Option<serde_json::Value> {
        if !self.is_explicit(has_tools) {
            return Option::None;
        }
        Some(match self {
            Self::Auto => serde_json::json!({ "mode": "AUTO" }),
            Self::Required => serde_json::json!({ "mode": "ANY" }),
            Self::None => serde_json::json!({ "mode": "NONE" }),
            Self::Specific { name } => serde_json::json!({
                "mode": "ANY",
                "allowed_function_names": [name],
            }),
        })
    }
}

#[cfg(test)]
mod tool_choice_wire_tests {
    use super::ToolChoice;

    #[test]
    fn should_omit_tool_choice_for_the_default_and_for_tool_less_requests() {
        assert!(ToolChoice::Auto.openai_chat_wire(true).is_none());
        assert!(ToolChoice::None.openai_chat_wire(false).is_none());
        assert!(ToolChoice::None.anthropic_wire(false).is_none());
        assert!(
            ToolChoice::None
                .gemini_function_calling_config(false)
                .is_none()
        );
        assert!(ToolChoice::None.openai_responses_wire(false).is_none());
    }

    #[test]
    fn should_render_every_provider_wire_form_for_explicit_choices() {
        assert_eq!(ToolChoice::None.openai_chat_wire(true).unwrap(), "none");
        assert_eq!(
            ToolChoice::Required.openai_chat_wire(true).unwrap(),
            "required"
        );
        assert_eq!(
            ToolChoice::Specific {
                name: "read".into()
            }
            .openai_chat_wire(true)
            .unwrap(),
            serde_json::json!({"type": "function", "function": {"name": "read"}})
        );
        assert_eq!(
            ToolChoice::Specific {
                name: "read".into()
            }
            .openai_responses_wire(true)
            .unwrap(),
            serde_json::json!({"type": "function", "name": "read"})
        );
        assert_eq!(
            ToolChoice::None.anthropic_wire(true).unwrap(),
            serde_json::json!({"type": "none"})
        );
        assert_eq!(
            ToolChoice::Required.anthropic_wire(true).unwrap(),
            serde_json::json!({"type": "any"})
        );
        assert_eq!(
            ToolChoice::None
                .gemini_function_calling_config(true)
                .unwrap(),
            serde_json::json!({"mode": "NONE"})
        );
        assert_eq!(
            ToolChoice::Specific {
                name: "read".into()
            }
            .gemini_function_calling_config(true)
            .unwrap(),
            serde_json::json!({"mode": "ANY", "allowed_function_names": ["read"]})
        );
    }
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
    fn test_chat_config_serde_roundtrip() {
        let config = ChatConfig {
            max_tokens: Some(2048),
            temperature: Some(0.7),
            tool_choice: ToolChoice::Required,
            stop_sequences: vec!["STOP".to_string()],
            reasoning_effort: None,
            response_format: None,
            context_management: None,
            prompt_cache_context: None,
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
            prompt_cache_context: None,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("stop_sequences").is_none());
        assert!(json.get("context_management").is_none());
        assert!(json.get("prompt_cache_context").is_none());
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
    fn deepest_shared_checkpoint_stops_before_edited_semantic_suffix() {
        fn hint(id: &str, hash: &str) -> SemanticCheckpointHint {
            SemanticCheckpointHint {
                boundary_id: id.to_owned(),
                boundary_kind: "message:user".to_owned(),
                prefix_hash: hash.to_owned(),
                prefix_token_estimate: 1,
                estimated_recompute_tokens: 1,
                checkpoint_priority: 1,
            }
        }
        let previous = PromptCacheContext {
            affinity_key: "key".to_owned(),
            epoch_id: "epoch".to_owned(),
            stable_prefix_hash: "stable".to_owned(),
            semantic_boundaries: vec![hint("one", "h1"), hint("two", "h2")],
        };
        let edited = PromptCacheContext {
            semantic_boundaries: vec![hint("one-new-id", "h1"), hint("two", "changed")],
            ..previous.clone()
        };

        assert_eq!(
            previous
                .deepest_shared_checkpoint(&edited)
                .map(|hint| hint.boundary_id.as_str()),
            Some("one")
        );
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
