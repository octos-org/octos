//! Error types for octos with actionable messages.

use std::fmt;

/// Result type for octos operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for octos operations.
#[derive(Debug)]
pub struct Error {
    /// The error kind.
    pub kind: ErrorKind,
    /// Additional context.
    pub context: Option<String>,
    /// Suggestion for fixing the error.
    pub suggestion: Option<String>,
}

/// Kinds of errors that can occur.
#[derive(Debug)]
pub enum ErrorKind {
    /// Task not found.
    TaskNotFound(String),
    /// Agent not found.
    AgentNotFound(String),
    /// Invalid task state transition.
    InvalidStateTransition { from: String, to: String },
    /// LLM provider error.
    LlmError { provider: String, message: String },
    /// API error with status code.
    ApiError {
        provider: String,
        status: u16,
        body: String,
    },
    /// Tool execution error.
    ToolError { tool: String, message: String },
    /// Configuration error.
    ConfigError(String),
    /// API key not set.
    ApiKeyNotSet { provider: String, env_var: String },
    /// Unknown provider.
    UnknownProvider(String),
    /// Timeout.
    Timeout { operation: String, seconds: u64 },
    /// Channel error (gateway).
    ChannelError { channel: String, message: String },
    /// Session error (gateway).
    SessionError(String),
    /// IO error.
    IoError(std::io::Error),
    /// Serialization error.
    SerializationError(String),
    /// Generic error with context.
    Other(eyre::Report),
}

impl Error {
    /// Create a new error from a kind.
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            context: None,
            suggestion: None,
        }
    }

    /// Add context to the error.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Add a suggestion to the error.
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    // Convenience constructors

    /// Task not found error.
    pub fn task_not_found(id: impl Into<String>) -> Self {
        Self::new(ErrorKind::TaskNotFound(id.into()))
            .with_suggestion("Run 'octos list' to see available tasks")
    }

    /// API key not set error.
    pub fn api_key_not_set(provider: impl Into<String>, env_var: impl Into<String>) -> Self {
        let provider = provider.into();
        let env_var = env_var.into();
        Self::new(ErrorKind::ApiKeyNotSet {
            provider: provider.clone(),
            env_var: env_var.clone(),
        })
        .with_suggestion(format!(
            "Set the API key:\n    export {env_var}=your-api-key\n  Or add to .octos/config.json:\n    {{\"api_key_env\": \"{env_var}\"}}"
        ))
    }

    /// Unknown provider error.
    pub fn unknown_provider(provider: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownProvider(provider.into()))
            .with_suggestion("Supported providers: 'anthropic', 'openai'")
    }

    /// API error.
    pub fn api_error(provider: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        let provider = provider.into();
        let body = body.into();

        let suggestion = match status {
            401 => "Check that your API key is valid and not expired",
            403 => "Check that your API key has the required permissions",
            429 => "Rate limited. Wait a moment and try again, or reduce request frequency",
            504 => "Gateway timeout. The provider may be overloaded, try again later",
            500..=599 => "Server error on provider side. Try again later",
            _ => "Check the provider's API documentation for this error code",
        };

        Self::new(ErrorKind::ApiError {
            provider,
            status,
            body,
        })
        .with_suggestion(suggestion)
    }

    /// Tool error.
    pub fn tool_error(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ToolError {
            tool: tool.into(),
            message: message.into(),
        })
    }

    /// Config error.
    pub fn config_error(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ConfigError(message.into()))
            .with_suggestion("Check that .octos/config.json is valid JSON")
    }

    /// Timeout error.
    pub fn timeout(operation: impl Into<String>, seconds: u64) -> Self {
        Self::new(ErrorKind::Timeout {
            operation: operation.into(),
            seconds,
        })
        .with_suggestion("Try increasing the timeout or simplifying the task")
    }

    /// LLM error.
    pub fn llm_error(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::LlmError {
            provider: provider.into(),
            message: message.into(),
        })
    }

    /// Channel error.
    pub fn channel_error(channel: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ChannelError {
            channel: channel.into(),
            message: message.into(),
        })
    }

    /// Session error.
    pub fn session_error(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::SessionError(message.into()))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Main message
        match &self.kind {
            ErrorKind::TaskNotFound(id) => write!(f, "Task not found: {id}")?,
            ErrorKind::AgentNotFound(id) => write!(f, "Agent not found: {id}")?,
            ErrorKind::InvalidStateTransition { from, to } => {
                write!(f, "Invalid state transition: {from} -> {to}")?
            }
            ErrorKind::LlmError { provider, message } => write!(f, "{provider} error: {message}")?,
            ErrorKind::ApiError {
                provider,
                status,
                body,
            } => {
                write!(f, "{provider} API error ({status})")?;
                if !body.is_empty() {
                    write!(f, ": {}", crate::truncated_utf8(body, 200, "..."))?;
                }
            }
            ErrorKind::ToolError { tool, message } => write!(f, "Tool '{tool}' failed: {message}")?,
            ErrorKind::ConfigError(msg) => write!(f, "Config error: {msg}")?,
            ErrorKind::ApiKeyNotSet { provider, env_var } => {
                write!(f, "{provider} API key not set ({env_var})")?
            }
            ErrorKind::UnknownProvider(p) => write!(f, "Unknown provider: {p}")?,
            ErrorKind::Timeout { operation, seconds } => {
                write!(f, "{operation} timed out after {seconds}s")?
            }
            ErrorKind::ChannelError { channel, message } => {
                write!(f, "Channel '{channel}' error: {message}")?
            }
            ErrorKind::SessionError(msg) => write!(f, "Session error: {msg}")?,
            ErrorKind::IoError(e) => write!(f, "IO error: {e}")?,
            ErrorKind::SerializationError(msg) => write!(f, "Serialization error: {msg}")?,
            ErrorKind::Other(e) => write!(f, "{e}")?,
        }

        // Context
        if let Some(ctx) = &self.context {
            write!(f, "\n  Context: {ctx}")?;
        }

        // Suggestion
        if let Some(sug) = &self.suggestion {
            write!(f, "\n  Suggestion: {sug}")?;
        }

        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::IoError(e))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::SerializationError(e.to_string()))
    }
}

impl From<eyre::Report> for Error {
    fn from(e: eyre::Report) -> Self {
        Self::new(ErrorKind::Other(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_truncates_long_body() {
        let long_body = "x".repeat(500);
        let err = Error::api_error("test", 400, &long_body);
        let msg = err.to_string();
        // Body should be truncated
        assert!(msg.contains("..."));
        assert!(msg.len() < long_body.len() + 100);
    }

    #[test]
    fn test_source_returns_io_error() {
        use std::error::Error as StdError;
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: Error = io_err.into();
        assert!(err.source().is_some());
    }
}
