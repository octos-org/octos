//! R-10 regression tests: deadline/backoff arithmetic on the real parser and
//! executor paths. Kept separate so `executor_tests.rs` stays under the
//! 3000-line quality-ratchet ceiling.

use super::tests::make_capped_config;
use super::*;
use crate::handler::Handler;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[tokio::test]
async fn fanout_worker_rejects_invalid_deadline_before_handler() {
    let handler = Arc::new(RetryErrorHandler {
        calls: AtomicUsize::new(0),
    });
    let handler_trait: Arc<dyn Handler> = handler.clone();
    let node = PipelineNode {
        id: "fanout-invalid-deadline".into(),
        deadline_secs: Some(0.0),
        ..Default::default()
    };

    let error = run_fanout_worker(&handler_trait, &node, &retry_test_context(), 0, None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("deadline_secs"));
    assert_eq!(handler.calls.load(AtomicOrdering::Relaxed), 0);
}

#[test]
fn retry_backoff_uses_checked_exponent_and_explicit_cap() {
    for (attempt, expected_secs) in [
        (0, 1),
        (1, 2),
        (4, 16),
        (5, MAX_RETRY_BACKOFF_SECS),
        (63, MAX_RETRY_BACKOFF_SECS),
        (64, MAX_RETRY_BACKOFF_SECS),
        (u32::MAX, MAX_RETRY_BACKOFF_SECS),
    ] {
        assert_eq!(retry_backoff(attempt), Duration::from_secs(expected_secs));
    }
    assert!(validate_retry_budget(0).is_ok());
    assert!(validate_retry_budget(MAX_NODE_RETRIES).is_ok());
    assert!(validate_retry_budget(MAX_NODE_RETRIES + 1).is_err());
}

struct RetryErrorHandler {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Handler for RetryErrorHandler {
    async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
        self.calls.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Error,
            content: String::new(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

fn retry_test_context() -> HandlerContext {
    HandlerContext {
        input: String::new(),
        completed: HashMap::new(),
        predecessor_outcomes: vec![],
        working_dir: PathBuf::from("/tmp"),
    }
}

#[tokio::test]
async fn dispatch_node_clamps_oversized_finite_deadline() {
    let executor = PipelineExecutor::new(make_capped_config(8).await);
    let node = PipelineNode {
        id: "deadline".into(),
        deadline_secs: Some(1e308),
        ..Default::default()
    };
    let handler: Arc<dyn Handler> = Arc::new(NoopHandler);

    let outcome = executor
        .dispatch_node(&handler, &node, &retry_test_context(), 0)
        .await
        .unwrap();

    assert!(matches!(outcome, DispatchOutcome::Completed(_)));
}

#[tokio::test(start_paused = true)]
async fn execute_with_retries_rejects_excessive_retry_budget_before_backoff_overflow() {
    let executor = PipelineExecutor::new(make_capped_config(8).await);
    let handler = Arc::new(RetryErrorHandler {
        calls: AtomicUsize::new(0),
    });
    let handler_trait: Arc<dyn Handler> = handler.clone();
    let node = PipelineNode {
        id: "retry".into(),
        ..Default::default()
    };

    let error = executor
        .execute_with_retries(&handler_trait, &node, &retry_test_context(), 65)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("max_retries"));
    assert_eq!(handler.calls.load(AtomicOrdering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn static_retries_reject_excessive_retry_budget_before_backoff_overflow() {
    let handler = Arc::new(RetryErrorHandler {
        calls: AtomicUsize::new(0),
    });
    let handler_trait: Arc<dyn Handler> = handler.clone();
    let node = PipelineNode {
        id: "retry-static".into(),
        ..Default::default()
    };

    let error = execute_with_retries_static(&handler_trait, &node, &retry_test_context(), 65)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("max_retries"));
    assert_eq!(handler.calls.load(AtomicOrdering::Relaxed), 0);
}
