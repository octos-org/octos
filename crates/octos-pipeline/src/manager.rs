//! Manager loop handler for child pipeline supervision.
//!
//! Implements the supervisor pattern: a manager node spawns and monitors
//! child pipelines, collecting their results and deciding next steps.
//!
//! TODO: Wire into executor to support `handler=manager` nodes.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::graph::{NodeOutcome, OutcomeStatus};

/// Specification for a child pipeline to be spawned by the manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSpec {
    /// Name of the child pipeline (used as identifier).
    pub name: String,
    /// DOT source or path to DOT file.
    pub pipeline: String,
    /// Input to pass to the child pipeline.
    pub input: String,
    /// Working directory override (defaults to parent's).
    pub working_dir: Option<PathBuf>,
}

/// Result from a completed child pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildResult {
    /// Name of the child pipeline.
    pub name: String,
    /// Whether it completed successfully.
    pub success: bool,
    /// Output text.
    pub output: String,
}

/// Strategy for how the manager handles child results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionStrategy {
    /// All children must succeed; fail on first failure.
    #[default]
    AllOrNothing,
    /// Continue even if some children fail; collect all results.
    BestEffort,
    /// Retry failed children up to `max_retries` times (capped at 10).
    RetryFailed { max_retries: u32 },
}

/// Trait for executing child pipelines.
#[async_trait]
pub trait ChildExecutor: Send + Sync {
    /// Execute a child pipeline and return its result.
    async fn execute_child(&self, spec: &ChildSpec) -> Result<ChildResult>;
}

/// Manager that supervises child pipeline execution.
pub struct PipelineManager {
    strategy: SupervisionStrategy,
    executor: Arc<dyn ChildExecutor>,
}

impl PipelineManager {
    pub fn new(strategy: SupervisionStrategy, executor: Arc<dyn ChildExecutor>) -> Self {
        Self { strategy, executor }
    }

    /// Run all child pipelines according to the supervision strategy.
    pub async fn run(&self, children: Vec<ChildSpec>) -> Result<ManagerOutcome> {
        let mut results = Vec::new();
        let mut failures = Vec::new();

        for spec in &children {
            let result = self.execute_with_strategy(spec).await;
            match result {
                Ok(r) => {
                    if !r.success {
                        failures.push(r.name.clone());
                        results.push(r);
                        if self.strategy == SupervisionStrategy::AllOrNothing {
                            break;
                        }
                        continue;
                    }
                    results.push(r);
                }
                Err(e) => {
                    failures.push(spec.name.clone());
                    results.push(ChildResult {
                        name: spec.name.clone(),
                        success: false,
                        output: format!("Error: {e}"),
                    });
                    if self.strategy == SupervisionStrategy::AllOrNothing {
                        break;
                    }
                }
            }
        }

        let success = match &self.strategy {
            SupervisionStrategy::AllOrNothing => failures.is_empty(),
            SupervisionStrategy::BestEffort => true,
            SupervisionStrategy::RetryFailed { .. } => failures.is_empty(),
        };

        Ok(ManagerOutcome {
            success,
            results,
            failures,
        })
    }

    /// Maximum allowed retries (prevents DoS from unbounded retry configs).
    const MAX_RETRY_CAP: u32 = 10;

    async fn execute_with_strategy(&self, spec: &ChildSpec) -> Result<ChildResult> {
        match &self.strategy {
            SupervisionStrategy::RetryFailed { max_retries } => {
                let capped = (*max_retries).min(Self::MAX_RETRY_CAP);
                let mut last_result = self.executor.execute_child(spec).await?;
                let mut attempts = 0;
                while !last_result.success && attempts < capped {
                    attempts += 1;
                    // Exponential backoff: 100ms, 200ms, 400ms, ... capped at 5s
                    let delay = std::time::Duration::from_millis(
                        (100 * (1u64 << attempts.min(6))).min(5000),
                    );
                    tokio::time::sleep(delay).await;
                    last_result = self.executor.execute_child(spec).await?;
                }
                Ok(last_result)
            }
            _ => self.executor.execute_child(spec).await,
        }
    }
}

/// Outcome of a manager's supervision run.
#[derive(Debug, Clone)]
pub struct ManagerOutcome {
    /// Overall success.
    pub success: bool,
    /// Results from all children.
    pub results: Vec<ChildResult>,
    /// Names of failed children.
    pub failures: Vec<String>,
}

impl ManagerOutcome {
    /// Convert to a NodeOutcome for use in the pipeline.
    pub fn to_node_outcome(&self, node_id: &str) -> NodeOutcome {
        let output = self
            .results
            .iter()
            .map(|r| {
                let status = if r.success { "pass" } else { "fail" };
                format!("[{}] {}: {}", status, r.name, r.output)
            })
            .collect::<Vec<_>>()
            .join("\n");

        NodeOutcome {
            node_id: node_id.to_string(),
            status: if self.success {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail
            },
            content: output,
            token_usage: Default::default(),
            files_modified: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockExecutor {
        results: HashMap<String, ChildResult>,
    }

    impl MockExecutor {
        fn new(results: Vec<ChildResult>) -> Self {
            Self {
                results: results.into_iter().map(|r| (r.name.clone(), r)).collect(),
            }
        }
    }

    #[async_trait]
    impl ChildExecutor for MockExecutor {
        async fn execute_child(&self, spec: &ChildSpec) -> Result<ChildResult> {
            self.results
                .get(&spec.name)
                .cloned()
                .ok_or_else(|| eyre::eyre!("unknown child: {}", spec.name))
        }
    }

    fn make_spec(name: &str) -> ChildSpec {
        ChildSpec {
            name: name.into(),
            pipeline: "digraph {}".into(),
            input: "test".into(),
            working_dir: None,
        }
    }

    #[tokio::test]
    async fn should_succeed_when_all_pass() {
        let executor = Arc::new(MockExecutor::new(vec![
            ChildResult {
                name: "a".into(),
                success: true,
                output: "ok".into(),
            },
            ChildResult {
                name: "b".into(),
                success: true,
                output: "ok".into(),
            },
        ]));
        let mgr = PipelineManager::new(SupervisionStrategy::AllOrNothing, executor);
        let outcome = mgr.run(vec![make_spec("a"), make_spec("b")]).await.unwrap();
        assert!(outcome.success);
        assert!(outcome.failures.is_empty());
    }

    #[tokio::test]
    async fn should_fail_on_first_failure_all_or_nothing() {
        let executor = Arc::new(MockExecutor::new(vec![
            ChildResult {
                name: "a".into(),
                success: false,
                output: "err".into(),
            },
            ChildResult {
                name: "b".into(),
                success: true,
                output: "ok".into(),
            },
        ]));
        let mgr = PipelineManager::new(SupervisionStrategy::AllOrNothing, executor);
        let outcome = mgr.run(vec![make_spec("a"), make_spec("b")]).await.unwrap();
        assert!(!outcome.success);
        assert!(outcome.failures.contains(&"a".to_string()));
        // AllOrNothing should stop after first failure — "b" not executed
        assert_eq!(outcome.results.len(), 1);
    }

    #[tokio::test]
    async fn should_continue_on_failure_best_effort() {
        let executor = Arc::new(MockExecutor::new(vec![
            ChildResult {
                name: "a".into(),
                success: false,
                output: "err".into(),
            },
            ChildResult {
                name: "b".into(),
                success: true,
                output: "ok".into(),
            },
        ]));
        let mgr = PipelineManager::new(SupervisionStrategy::BestEffort, executor);
        let outcome = mgr.run(vec![make_spec("a"), make_spec("b")]).await.unwrap();
        assert!(outcome.success); // best effort always "succeeds"
        assert_eq!(outcome.results.len(), 2);
    }

    #[tokio::test]
    async fn should_convert_to_node_outcome() {
        let outcome = ManagerOutcome {
            success: true,
            results: vec![ChildResult {
                name: "a".into(),
                success: true,
                output: "done".into(),
            }],
            failures: vec![],
        };
        let node = outcome.to_node_outcome("manager_node");
        assert_eq!(node.node_id, "manager_node");
        assert_eq!(node.status, OutcomeStatus::Pass);
        assert!(node.content.contains("[pass] a: done"));
    }

    #[test]
    fn should_default_to_all_or_nothing() {
        assert_eq!(
            SupervisionStrategy::default(),
            SupervisionStrategy::AllOrNothing
        );
    }
}
