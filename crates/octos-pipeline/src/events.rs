//! Structured observability events for pipeline execution.
//!
//! Provides a typed event system that pipeline executors emit at key lifecycle
//! points. Consumers can subscribe via the `PipelineEventHandler` trait.

use octos_core::TokenUsage;
use serde::Serialize;

use crate::graph::{HandlerKind, OutcomeStatus};

/// A structured pipeline lifecycle event.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// Pipeline execution has started.
    PipelineStarted {
        graph_id: String,
        node_count: usize,
        edge_count: usize,
    },
    /// A node has begun executing.
    NodeStarted {
        node_id: String,
        handler: HandlerKind,
        model: Option<String>,
        label: Option<String>,
    },
    /// A node has finished executing.
    NodeCompleted {
        node_id: String,
        status: OutcomeStatus,
        duration_ms: u64,
        token_usage: TokenUsage,
    },
    /// An edge was selected for traversal.
    EdgeSelected {
        from: String,
        to: String,
        reason: String,
    },
    /// Parallel fan-out started.
    ParallelFanOut {
        node_id: String,
        targets: Vec<String>,
        converge: String,
    },
    /// Parallel fan-out converged.
    ParallelConverged {
        node_id: String,
        converge: String,
        success: bool,
        duration_ms: u64,
    },
    /// Pipeline execution has completed.
    PipelineCompleted {
        graph_id: String,
        success: bool,
        duration_ms: u64,
        total_tokens: TokenUsage,
        nodes_executed: usize,
    },
}

/// Trait for receiving pipeline events.
///
/// Implementations can log, persist, or forward events as needed.
pub trait PipelineEventHandler: Send + Sync {
    fn on_event(&self, event: &PipelineEvent);
}

/// Event handler that logs events via `tracing`.
pub struct TracingEventHandler;

impl PipelineEventHandler for TracingEventHandler {
    fn on_event(&self, event: &PipelineEvent) {
        match event {
            PipelineEvent::PipelineStarted {
                graph_id,
                node_count,
                edge_count,
            } => {
                tracing::info!(
                    graph = %graph_id,
                    nodes = node_count,
                    edges = edge_count,
                    "pipeline started"
                );
            }
            PipelineEvent::NodeStarted {
                node_id,
                handler,
                model,
                label,
            } => {
                tracing::info!(
                    node = %node_id,
                    handler = ?handler,
                    model = ?model,
                    label = ?label,
                    "node started"
                );
            }
            PipelineEvent::NodeCompleted {
                node_id,
                status,
                duration_ms,
                token_usage,
            } => {
                tracing::info!(
                    node = %node_id,
                    status = ?status,
                    duration_ms,
                    input_tokens = token_usage.input_tokens,
                    output_tokens = token_usage.output_tokens,
                    "node completed"
                );
            }
            PipelineEvent::EdgeSelected { from, to, reason } => {
                tracing::debug!(from = %from, to = %to, reason = %reason, "edge selected");
            }
            PipelineEvent::ParallelFanOut {
                node_id,
                targets,
                converge,
            } => {
                tracing::info!(
                    node = %node_id,
                    targets = ?targets,
                    converge = %converge,
                    "parallel fan-out"
                );
            }
            PipelineEvent::ParallelConverged {
                node_id,
                converge,
                success,
                duration_ms,
            } => {
                tracing::info!(
                    node = %node_id,
                    converge = %converge,
                    success,
                    duration_ms,
                    "parallel converged"
                );
            }
            PipelineEvent::PipelineCompleted {
                graph_id,
                success,
                duration_ms,
                total_tokens,
                nodes_executed,
            } => {
                tracing::info!(
                    graph = %graph_id,
                    success,
                    duration_ms,
                    input_tokens = total_tokens.input_tokens,
                    output_tokens = total_tokens.output_tokens,
                    nodes_executed,
                    "pipeline completed"
                );
            }
        }
    }
}

/// Event handler that collects events into a Vec (useful for testing).
pub struct CollectingEventHandler {
    events: std::sync::Mutex<Vec<PipelineEvent>>,
}

impl CollectingEventHandler {
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<PipelineEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Default for CollectingEventHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineEventHandler for CollectingEventHandler {
    fn on_event(&self, event: &PipelineEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_serialize_pipeline_started() {
        let event = PipelineEvent::PipelineStarted {
            graph_id: "test_pipeline".into(),
            node_count: 5,
            edge_count: 4,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "pipeline_started");
        assert_eq!(json["graph_id"], "test_pipeline");
    }

    #[test]
    fn should_serialize_node_completed() {
        let event = PipelineEvent::NodeCompleted {
            node_id: "analyze".into(),
            status: OutcomeStatus::Pass,
            duration_ms: 1500,
            token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "node_completed");
        assert_eq!(json["status"], "pass");
        assert_eq!(json["duration_ms"], 1500);
    }

    #[test]
    fn should_collect_events() {
        let handler = CollectingEventHandler::new();
        handler.on_event(&PipelineEvent::PipelineStarted {
            graph_id: "g1".into(),
            node_count: 3,
            edge_count: 2,
        });
        handler.on_event(&PipelineEvent::PipelineCompleted {
            graph_id: "g1".into(),
            success: true,
            duration_ms: 5000,
            total_tokens: TokenUsage::default(),
            nodes_executed: 3,
        });
        let events = handler.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], PipelineEvent::PipelineStarted { .. }));
        assert!(matches!(
            &events[1],
            PipelineEvent::PipelineCompleted { .. }
        ));
    }

    #[test]
    fn should_serialize_edge_selected() {
        let event = PipelineEvent::EdgeSelected {
            from: "a".into(),
            to: "b".into(),
            reason: "condition matched".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "edge_selected");
        assert_eq!(json["from"], "a");
        assert_eq!(json["to"], "b");
    }

    #[test]
    fn should_serialize_parallel_fan_out() {
        let event = PipelineEvent::ParallelFanOut {
            node_id: "p1".into(),
            targets: vec!["w1".into(), "w2".into()],
            converge: "merge".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "parallel_fan_out");
        assert_eq!(json["targets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn should_handle_tracing_handler() {
        // Just verify it doesn't panic
        let handler = TracingEventHandler;
        handler.on_event(&PipelineEvent::PipelineStarted {
            graph_id: "test".into(),
            node_count: 1,
            edge_count: 0,
        });
    }
}
