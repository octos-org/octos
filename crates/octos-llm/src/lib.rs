//! LLM provider abstraction for octos.
//!
//! This crate provides a unified interface for interacting with LLM providers:
//! - Anthropic (Claude)
//! - OpenAI (GPT-4)
//! - Google Gemini
//! - Ollama (local models)

pub mod adaptive;
mod config;
pub mod context;
mod context_override;
pub mod embedding;
mod failover;
mod fallback;
pub mod pricing;
mod provider;
pub mod responsiveness;
mod retry;
pub mod router;
pub mod sse;
pub mod stream_accumulator;
mod swappable;
mod types;
pub mod vision;

pub mod catalog;
pub mod error;
pub mod high_level;
pub mod middleware;

pub mod anthropic;
pub mod gemini;
pub mod ominix;
pub mod openai;
pub mod openai_responses;
pub mod openrouter;
pub mod registry;

pub use adaptive::{
    AdaptiveConfig, AdaptiveMode, AdaptiveRouter, AdaptiveStatus, BaselineEntry, MetricsSnapshot,
    ModelCatalogEntry, ModelType, QosCatalog, SharedMetrics, SharedPolicy, SharedProviderMetrics,
    StatusCallback,
};
pub use catalog::{ModelCapabilities, ModelCatalog, ModelCost, ModelInfo};
pub use config::{ChatConfig, ResponseFormat, ToolChoice};
pub use context_override::ContextWindowOverride;
pub use embedding::{EmbeddingProvider, OpenAIEmbedder};
pub use error::{LlmError, LlmErrorKind};
pub use failover::ProviderChain;
pub use fallback::FallbackProvider;
pub use high_level::LlmClient;
pub use middleware::{LlmMiddleware, MiddlewareStack};
pub use ominix::{OminixClient, PlatformModels};
pub use provider::{
    DEFAULT_EMBEDDING_CONNECT_TIMEOUT_SECS, DEFAULT_EMBEDDING_TIMEOUT_SECS,
    DEFAULT_LLM_CONNECT_TIMEOUT_SECS, DEFAULT_LLM_TIMEOUT_SECS, LlmProvider, build_http_client,
};
pub use responsiveness::ResponsivenessObserver;
pub use retry::{RetryConfig, RetryProvider};
pub use router::{ProviderRouter, SubProviderMeta};
pub use stream_accumulator::StreamAccumulator;
pub use swappable::SwappableProvider;
pub use types::{
    ChatResponse, ChatStream, StopReason, StreamEvent, TokenUsage, ToolSpec, strip_think_tags,
};
