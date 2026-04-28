//! M8.9 recovery loop for pipeline node execution (W1.A2).
//!
//! When a pipeline node fails with a retryable error the executor
//! re-engages the node ONCE through this module. The recovery prompt is
//! synthesised from the failure signal in the same shape as the
//! `session_actor::build_recovery_prompt` function used by spawn_only
//! tasks — same framing, same alternative-list format — so the LLM
//! sees a uniform recovery contract no matter where it is invoked
//! from.
//!
//! The wrapper is intentionally thin: it does not own the dispatch
//! loop; the caller invokes [`recover_node`] only when its own
//! dispatch path returns a retryable failure. The single retry budget
//! is enforced here (a node that fails twice in a row bubbles up).
//!
//! Design invariants:
//! * Non-retryable failures bubble up unchanged — no infinite retry.
//! * Recovery is bounded to a single attempt; the second failure is
//!   terminal.
//! * The recovery prompt is appended to the node's existing prompt;
//!   the original prompt is not lost.
//! * Cancellation (shutdown signal set) skips recovery entirely so the
//!   pipeline tears down promptly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eyre::Result;
use serde_json::Value;

use crate::graph::{NodeOutcome, OutcomeStatus, PipelineNode};
use crate::handler::{Handler, HandlerContext};

/// Decision returned by the executor when a node finishes its first
/// dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// The node passed; no recovery needed.
    Pass,
    /// The node failed in a way the M8.9 contract considers retryable.
    /// A recovery prompt should be built and the node re-engaged once.
    Retryable(Box<RetryableSignal>),
    /// The node failed in a way that is terminal — bubble up as-is.
    Terminal,
}

/// Failure signal threaded into [`build_recovery_prompt`]. Mirrors
/// `octos_agent::SpawnOnlyFailureSignal` in shape so the LLM sees the
/// same recovery contract whether it is recovering a spawn_only task
/// or a pipeline node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryableSignal {
    /// Logical name surfaced to the model. Use the node id so the
    /// prompt is anchored to a recognisable label.
    pub node_id: String,
    /// Short error message extracted from the failed [`NodeOutcome`].
    pub error_message: String,
    /// Optional alternative paths (e.g. "use a different model",
    /// "skip the search step"). Empty when none are known.
    pub suggested_alternatives: Vec<String>,
    /// Original input to the node, surfaced verbatim so the model can
    /// re-attempt with the same data.
    pub original_input: Value,
}

impl RetryableSignal {
    /// Construct a retryable failure signal from a failed node outcome.
    /// Convenience helper used by [`classify_outcome`].
    pub fn from_outcome(node: &PipelineNode, outcome: &NodeOutcome, input: Value) -> Self {
        Self {
            node_id: node.id.clone(),
            error_message: outcome.content.clone(),
            suggested_alternatives: extract_suggested_alternatives(&outcome.content),
            original_input: input,
        }
    }
}

/// Outcome of a recovery attempt, returned by [`recover_node`].
#[derive(Debug, Clone)]
pub struct RecoveryOutcome {
    /// Final node outcome — either the recovered Pass outcome OR the
    /// second-failure outcome from the retry attempt.
    pub outcome: NodeOutcome,
    /// `true` when the recovery prompt was actually engaged (i.e. a
    /// retry happened). `false` when the input outcome was Pass /
    /// Skipped / Cancelled and recovery short-circuited.
    pub retried: bool,
}

/// Classify a node's outcome to decide whether a retry should fire.
/// Pass outcomes short-circuit; cancellations are terminal; everything
/// else maps to retryable today (the executor budget caps the retries
/// at one regardless).
pub fn classify_outcome(
    node: &PipelineNode,
    outcome: &NodeOutcome,
    input: &Value,
) -> RecoveryDecision {
    match outcome.status {
        OutcomeStatus::Pass => RecoveryDecision::Pass,
        OutcomeStatus::Skipped => RecoveryDecision::Terminal,
        OutcomeStatus::Fail | OutcomeStatus::Error => RecoveryDecision::Retryable(Box::new(
            RetryableSignal::from_outcome(node, outcome, input.clone()),
        )),
    }
}

/// Build the synthetic recovery prompt the LLM sees on the second
/// attempt. Matches the framing of
/// `session_actor::build_recovery_prompt` so spawn_only and pipeline
/// recovery look identical to the model.
pub fn build_recovery_prompt(signal: &RetryableSignal) -> String {
    let alternatives_block = if signal.suggested_alternatives.is_empty() {
        String::new()
    } else {
        let list = signal
            .suggested_alternatives
            .iter()
            .map(|alt| format!("- {alt}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\nDetected alternatives:\n{list}\n")
    };
    let input_block = if signal.original_input.is_null() {
        String::new()
    } else {
        let pretty = serde_json::to_string(&signal.original_input).unwrap_or_else(|_| "{}".into());
        format!("\nOriginal input: {pretty}")
    };
    format!(
        "[system-internal] Pipeline node `{node}` failed on the first attempt.\n\
         Error: {err}{input}{alts}\n\
         Re-engage this node with a recovery plan: try the safest alternative, \
         simplify the request, or surface a clear blocker. Do not just repeat \
         the same approach.",
        node = signal.node_id,
        err = signal.error_message,
        input = input_block,
        alts = alternatives_block,
    )
}

/// Run a single recovery attempt for a failed node. The caller has
/// already obtained the first-failure [`NodeOutcome`]; this function
/// builds the recovery prompt, invokes the handler ONCE more with the
/// augmented prompt, and returns the resulting outcome.
///
/// The shutdown flag is consulted before re-engagement so cancellation
/// during a stuck pipeline doesn't burn an extra dispatch.
pub async fn recover_node(
    handler: &Arc<dyn Handler>,
    node: &PipelineNode,
    base_ctx: &HandlerContext,
    signal: &RetryableSignal,
    shutdown: &Arc<AtomicBool>,
) -> Result<RecoveryOutcome> {
    if shutdown.load(Ordering::Acquire) {
        // Cancelled — pass through the original failure as Error so
        // the executor's normal terminal-error handling drives the
        // pipeline tear-down. We mark `retried = false` so callers
        // see we did not engage a second dispatch.
        return Ok(RecoveryOutcome {
            outcome: NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!(
                    "Recovery skipped for node '{}': pipeline shutdown signal raised before retry.",
                    node.id
                ),
                token_usage: octos_core::TokenUsage::default(),
                files_modified: vec![],
            },
            retried: false,
        });
    }

    let mut recovery_node = node.clone();
    let recovery_prompt = build_recovery_prompt(signal);
    let augmented = match recovery_node.prompt.take() {
        Some(existing) => format!("{existing}\n\n{recovery_prompt}"),
        None => recovery_prompt,
    };
    recovery_node.prompt = Some(augmented);

    tracing::warn!(
        node = %node.id,
        first_error = %signal.error_message,
        "M8.9 pipeline recovery: re-engaging node with recovery prompt"
    );

    let outcome = handler.execute(&recovery_node, base_ctx).await?;

    Ok(RecoveryOutcome {
        outcome,
        retried: true,
    })
}

/// Heuristic to extract suggested alternatives from a node's failure
/// content. Looks for lines that start with "Try " / "Use " / "-" /
/// "Alternative:" patterns. Returns an empty vector when nothing
/// matches — keeps recovery prompts short and focused.
fn extract_suggested_alternatives(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("- ")
            || trimmed.starts_with("Try ")
            || trimmed.starts_with("Use ")
            || trimmed.starts_with("Alternative:")
        {
            // Strip leading bullet so the prompt formatter can re-add
            // its own consistent bullet style.
            let cleaned = trimmed
                .trim_start_matches("- ")
                .trim_start_matches("Alternative:")
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
        if out.len() >= 3 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_core::TokenUsage;
    use std::sync::atomic::AtomicU32;

    /// Minimal handler that fails the first time and passes on retry.
    struct FailThenPass {
        attempts: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Handler for FailThenPass {
        async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(NodeOutcome {
                    node_id: node.id.clone(),
                    status: OutcomeStatus::Error,
                    content: "transient: connection refused".into(),
                    token_usage: TokenUsage::default(),
                    files_modified: vec![],
                })
            } else {
                Ok(NodeOutcome {
                    node_id: node.id.clone(),
                    status: OutcomeStatus::Pass,
                    content: format!("recovered on attempt {n}"),
                    token_usage: TokenUsage::default(),
                    files_modified: vec![],
                })
            }
        }
    }

    fn dummy_node(id: &str) -> PipelineNode {
        PipelineNode {
            id: id.into(),
            prompt: Some("original prompt".into()),
            ..Default::default()
        }
    }

    fn dummy_ctx() -> HandlerContext {
        HandlerContext {
            input: "input".into(),
            completed: Default::default(),
            working_dir: std::env::temp_dir(),
        }
    }

    #[test]
    fn classify_pass_returns_pass() {
        let node = dummy_node("n1");
        let outcome = NodeOutcome {
            node_id: "n1".into(),
            status: OutcomeStatus::Pass,
            content: "ok".into(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        };
        let decision = classify_outcome(&node, &outcome, &Value::Null);
        assert_eq!(decision, RecoveryDecision::Pass);
    }

    #[test]
    fn classify_error_returns_retryable() {
        let node = dummy_node("n1");
        let outcome = NodeOutcome {
            node_id: "n1".into(),
            status: OutcomeStatus::Error,
            content: "transient".into(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        };
        let decision = classify_outcome(&node, &outcome, &Value::Null);
        assert!(matches!(decision, RecoveryDecision::Retryable(_)));
    }

    #[test]
    fn build_recovery_prompt_includes_node_id_and_error() {
        let signal = RetryableSignal {
            node_id: "search".into(),
            error_message: "connection refused".into(),
            suggested_alternatives: vec!["retry with smaller batch".into()],
            original_input: serde_json::json!({"query": "hi"}),
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(prompt.contains("search"));
        assert!(prompt.contains("connection refused"));
        assert!(prompt.contains("retry with smaller batch"));
        assert!(prompt.contains("query"));
    }

    #[test]
    fn build_recovery_prompt_no_alternatives_omits_block() {
        let signal = RetryableSignal {
            node_id: "n".into(),
            error_message: "boom".into(),
            suggested_alternatives: vec![],
            original_input: Value::Null,
        };
        let prompt = build_recovery_prompt(&signal);
        assert!(!prompt.contains("Detected alternatives"));
    }

    #[tokio::test]
    async fn recover_node_retries_once_and_passes() {
        let attempts = Arc::new(AtomicU32::new(0));
        let handler: Arc<dyn Handler> = Arc::new(FailThenPass {
            attempts: attempts.clone(),
        });
        let node = dummy_node("n1");
        let ctx = dummy_ctx();
        // Mirror the executor: drive the first dispatch BEFORE
        // entering recover_node — recover_node only owns the second
        // attempt.
        let first = handler.execute(&node, &ctx).await.expect("first dispatch");
        assert_eq!(first.status, OutcomeStatus::Error);
        let signal = RetryableSignal::from_outcome(&node, &first, Value::Null);
        let shutdown = Arc::new(AtomicBool::new(false));
        let outcome = recover_node(&handler, &node, &ctx, &signal, &shutdown)
            .await
            .expect("recovery completes");
        assert!(outcome.retried);
        assert_eq!(outcome.outcome.status, OutcomeStatus::Pass);
        // First dispatch + recovery attempt = 2 invocations.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn recover_node_skips_when_shutdown_set() {
        let attempts = Arc::new(AtomicU32::new(0));
        let handler: Arc<dyn Handler> = Arc::new(FailThenPass {
            attempts: attempts.clone(),
        });
        let node = dummy_node("n1");
        let ctx = dummy_ctx();
        let signal = RetryableSignal {
            node_id: "n1".into(),
            error_message: "first".into(),
            suggested_alternatives: vec![],
            original_input: Value::Null,
        };
        let shutdown = Arc::new(AtomicBool::new(true));
        let outcome = recover_node(&handler, &node, &ctx, &signal, &shutdown)
            .await
            .expect("returns ok with cancelled-shaped outcome");
        assert!(!outcome.retried);
        // Mapped to Error since OutcomeStatus has no Cancelled variant.
        assert_eq!(outcome.outcome.status, OutcomeStatus::Error);
        assert!(outcome.outcome.content.contains("shutdown signal"));
        // Handler must not have been invoked.
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn extract_suggested_alternatives_picks_up_bullets() {
        let content = "transient: connection refused\n- retry with smaller batch\n- use cached results\nrandom noise";
        let alts = extract_suggested_alternatives(content);
        assert_eq!(alts.len(), 2);
        assert_eq!(alts[0], "retry with smaller batch");
    }
}
