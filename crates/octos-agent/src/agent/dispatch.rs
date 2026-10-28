//! Resolve fixed dispatch before native admission, never rewrite the transcript.
use super::Agent;
use octos_core::ToolCall;
use octos_llm::ChatResponse;

impl Agent {
    pub(super) fn effective_tool_call(&self, call: &ToolCall) -> ToolCall {
        let permissions = self
            .profile
            .as_deref()
            .map(crate::tools::ToolPermissions::from_profile)
            .unwrap_or_default();
        // Invalid calls remain wrappers so ordinary execution returns a tool
        // error with the original ID. They must not acquire a native identity.
        self.tools
            .resolve_dispatch_call(call, &permissions)
            .unwrap_or_else(|_| call.clone())
    }

    pub(super) fn effective_tool_response(&self, response: &ChatResponse) -> ChatResponse {
        let mut resolved = response.clone();
        resolved.tool_calls = response
            .tool_calls
            .iter()
            .map(|call| self.effective_tool_call(call))
            .collect();
        resolved
    }
}
