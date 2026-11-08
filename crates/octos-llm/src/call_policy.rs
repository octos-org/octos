//! Per-turn LLM call policy. A voice turn wraps its agent run in
//! `with_llm_call_policy(FailFast, ..)` so every provider wrapper and the
//! leaf provider short-circuit retry / failover / hedge — low latency wins
//! over robustness for spoken replies. Defaults to `Normal` (full retry).

/// Per-turn retry/failover policy read by the provider stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LlmCallPolicy {
    /// Normal: full retry ladder + failover + hedge (text chat).
    #[default]
    Normal,
    /// Fail-fast: at most one provider attempt; no retry, no failover, no hedge.
    FailFast,
}

tokio::task_local! {
    /// Per-turn call policy. Defaults to `Normal` when the caller hasn't
    /// wrapped the chat future. Mirrors `lane::LANE_CONTEXT` in shape.
    pub static CALL_POLICY: LlmCallPolicy;
}

/// Run `fut` inside a [`CALL_POLICY`] scope.
pub async fn with_llm_call_policy<F, T>(policy: LlmCallPolicy, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CALL_POLICY.scope(policy, fut).await
}

/// Snapshot the active policy. Returns `Normal` when no scope is active.
pub fn current_llm_call_policy() -> LlmCallPolicy {
    CALL_POLICY.try_with(|p| *p).unwrap_or_default()
}
