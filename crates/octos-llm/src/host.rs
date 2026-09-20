//! Versioned messages for a host-managed agent's model and tool broker.
//!
//! The host binds the connection to one compartment. These requests never carry
//! credentials, a destination URL, an account identity, or caller-defined labels.
//! A host must enforce its policy on every request, including compaction calls.

use octos_core::Message;
use serde::{Deserialize, Serialize};

use crate::{ChatConfig, ToolSpec};

pub const VERSION: u32 = 1;
pub const CAPABILITY_KEY: &str = "octos.hostManaged";
pub const MODEL_METHOD: &str = "_octos/host/model";
pub const TOOLS_LIST_METHOD: &str = "_octos/host/tools/list";
pub const TOOLS_CALL_METHOD: &str = "_octos/host/tools/call";
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 4096;
pub const MAX_TOOLS: usize = 256;
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 1024 * 1024;

/// Sent in `initialize.clientCapabilities._meta[CAPABILITY_KEY]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub version: u32,
    pub model: HostModel,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostModel {
    pub model_id: String,
    pub provider_name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl HostConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != VERSION {
            return Err("unsupported host broker version");
        }
        if self.model.model_id.is_empty()
            || self.model.model_id.len() > 1024
            || self.model.provider_name.is_empty()
            || self.model.provider_name.len() > 1024
            || self.model.context_window == 0
            || self.model.max_output_tokens == 0
            || self.model.max_output_tokens > self.model.context_window
        {
            return Err("invalid host model metadata");
        }
        if self.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
            return Err("host system prompt is too large");
        }
        Ok(())
    }
}

/// Returned in `initialize.agentCapabilities._meta[CAPABILITY_KEY]` only
/// after OS confinement succeeds. This reports startup state, not attestation
/// of an arbitrary executable: the host must launch a trusted Octos binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilities {
    pub version: u32,
    pub confined: bool,
    pub sandbox: String,
}

/// The result is an ordinary `ChatResponse`. The host chooses the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub config: ChatConfig,
}

impl ModelRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.messages.len() > MAX_MESSAGES || self.tools.len() > MAX_TOOLS {
            return Err("host model request exceeds item limits");
        }
        validate_payload_size(self)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsListRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsListResponse {
    pub tools: Vec<ToolSpec>,
}

impl ToolsListResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tools.len() > MAX_TOOLS {
            return Err("host tool list exceeds item limits");
        }
        let mut names = std::collections::HashSet::new();
        for tool in &self.tools {
            if tool.name.is_empty() || tool.name.len() > 256 || !names.insert(&tool.name) {
                return Err("invalid or duplicate host tool name");
            }
        }
        validate_payload_size(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRequest {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallResponse {
    pub content: String,
    pub is_error: bool,
}

/// Preserve reasoning and provider tool metadata when adapting a brokered
/// completion to the agent's streaming interface. Version 1 transports one
/// bounded completion per request; it does not open another network stream.
pub fn response_stream(response: crate::ChatResponse) -> crate::ChatStream {
    use crate::StreamEvent;
    let mut events = Vec::new();
    if let Some(index) = response.provider_index {
        events.push(StreamEvent::ProviderIndex(index));
    }
    if let Some(reasoning) = response.reasoning_content {
        events.push(StreamEvent::ReasoningDelta(reasoning));
    }
    if let Some(content) = response.content {
        events.push(StreamEvent::TextDelta(content));
    }
    for (index, tool) in response.tool_calls.into_iter().enumerate() {
        events.push(StreamEvent::ToolCallDelta {
            index,
            id: Some(tool.id),
            name: Some(tool.name),
            arguments_delta: tool.arguments.to_string(),
        });
        if let Some(metadata) = tool.metadata {
            events.push(StreamEvent::ToolCallMetadata { index, metadata });
        }
    }
    events.push(StreamEvent::Usage(response.usage));
    events.push(StreamEvent::Done(response.stop_reason));
    Box::pin(futures::stream::iter(events))
}

/// Bound JSON payloads without allocating a second serialized copy. Reserve
/// space for the surrounding JSON-RPC envelope and request identifier.
pub fn validate_payload_size(value: &impl Serialize) -> Result<(), &'static str> {
    struct LimitedWriter(usize);
    impl std::io::Write for LimitedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            if self.0 > MAX_FRAME_BYTES - 1024 {
                return Err(std::io::Error::other("host broker payload is too large"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(LimitedWriter(0), value)
        .map_err(|_| "invalid or oversized host broker payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_version_confusion_and_caller_supplied_identity() {
        let mut value = serde_json::json!({
            "version": VERSION,
            "model": {"model_id":"test", "provider_name":"host", "context_window":4096, "max_output_tokens":1024},
            "system_prompt":"Assistant"
        });
        let config: HostConfig = serde_json::from_value(value.clone()).unwrap();
        assert!(config.validate().is_ok());
        value["version"] = serde_json::json!(VERSION + 1);
        assert!(
            serde_json::from_value::<HostConfig>(value.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        value["account"] = serde_json::json!("attacker");
        assert!(serde_json::from_value::<HostConfig>(value).is_err());
        assert!(
            serde_json::from_value::<ToolCallRequest>(serde_json::json!({
                "name":"read", "arguments":{}, "labels":[]
            }))
            .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_tools_and_oversized_payloads() {
        let spec = ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: serde_json::json!({}),
        };
        assert!(
            ToolsListResponse {
                tools: vec![spec.clone()]
            }
            .validate()
            .is_ok()
        );
        assert!(
            ToolsListResponse {
                tools: vec![spec.clone(), spec]
            }
            .validate()
            .is_err()
        );
        assert!(validate_payload_size(&"x".repeat(MAX_FRAME_BYTES)).is_err());
    }
}
