//! Conversational hosts must retain partial work without treating truncation as success.

use octos_agent::{ConversationResponse, IncompleteResponseError};

pub(crate) enum ConversationOutcome {
    Complete(ConversationResponse),
    Incomplete { partial: ConversationResponse },
    Failed(eyre::Report),
}

pub(crate) const INCOMPLETE_NOTICE: &str =
    "Model output was truncated (max_tokens); the response is incomplete";

impl ConversationOutcome {
    pub(crate) fn from_result(result: eyre::Result<ConversationResponse>) -> Self {
        match result {
            Ok(response) => Self::Complete(response),
            Err(error) => match error.downcast_ref::<IncompleteResponseError>() {
                Some(incomplete) => Self::Incomplete {
                    partial: incomplete.partial.clone(),
                },
                None => Self::Failed(error),
            },
        }
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        matches!(self, Self::Incomplete { .. })
    }

    pub(crate) fn response(&self) -> Option<&ConversationResponse> {
        match self {
            Self::Complete(response) | Self::Incomplete { partial: response } => Some(response),
            Self::Failed(_) => None,
        }
    }
}

/// Display only: the persisted message remains the actual provider output.
pub(crate) fn display_incomplete(content: String, incomplete: bool) -> String {
    if !incomplete {
        return content;
    }
    if content.is_empty() {
        return format!("Error: {INCOMPLETE_NOTICE}");
    }
    format!("{content}\n\nError: {INCOMPLETE_NOTICE}")
}

pub(crate) fn mark_incomplete(metadata: &mut serde_json::Value, incomplete: bool) {
    if incomplete {
        metadata["outcome"] = "incomplete".into();
        metadata["truncated"] = true.into();
        metadata["error_code"] = "max_tokens".into();
        metadata["error"] = INCOMPLETE_NOTICE.into();
    }
}

pub(crate) fn mark_incomplete_usage(
    metadata: &mut serde_json::Value,
    usage: &octos_core::TokenUsage,
) {
    mark_incomplete(metadata, true);
    metadata["tokens_in"] = usage.input_tokens.into();
    metadata["tokens_out"] = usage.output_tokens.into();
    metadata["cache_read_tokens"] = usage.cache_read_tokens.into();
    metadata["cache_write_tokens"] = usage.cache_write_tokens.into();
    metadata["reasoning_tokens"] = usage.reasoning_tokens.into();
}
