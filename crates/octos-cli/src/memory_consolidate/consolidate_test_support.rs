//! A scripted merge model shared by the consolidation test modules.

use std::sync::atomic::{AtomicU32, Ordering};

use eyre::Result;
use octos_core::Message;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use tokio::sync::Notify;

/// Answers every merge call with the same reply and a small token usage,
/// counting calls. A gated one announces each call on `started` and waits
/// for `release`; a failing one returns a provider error instead.
pub(crate) struct ScriptedMerge {
    reply: Option<&'static str>,
    gated: bool,
    calls: AtomicU32,
    pub started: Notify,
    pub release: Notify,
}

impl ScriptedMerge {
    pub fn new(reply: &'static str) -> Self {
        Self {
            reply: Some(reply),
            gated: false,
            calls: AtomicU32::new(0),
            started: Notify::new(),
            release: Notify::new(),
        }
    }

    pub fn gated(reply: &'static str) -> Self {
        Self {
            gated: true,
            ..Self::new(reply)
        }
    }

    pub fn failing() -> Self {
        Self {
            reply: None,
            ..Self::new("")
        }
    }

    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedMerge {
    fn model_id(&self) -> &str {
        "scripted-merge"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }

    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.gated {
            self.started.notify_one();
            self.release.notified().await;
        }
        let reply = self
            .reply
            .ok_or_else(|| eyre::eyre!("HTTP 400: model not supported"))?;
        Ok(ChatResponse {
            content: Some(reply.to_owned()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                ..TokenUsage::default()
            },
            provider_index: None,
        })
    }
}
