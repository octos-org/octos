//! Prometheus metrics endpoint and helpers.

use std::sync::Arc;

use axum::extract::State;
use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use super::AppState;

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
pub fn init_metrics() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder")
}

/// GET /metrics -- render Prometheus text exposition format.
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    match state.metrics_handle {
        Some(ref handle) => handle.render(),
        None => String::new(),
    }
}

/// Record a tool call metric.
pub fn record_tool_call(name: &str, success: bool, duration_secs: f64) {
    let labels = [("tool", name.to_string()), ("success", success.to_string())];
    counter!("octos_tool_calls_total", &labels).increment(1);
    histogram!("octos_tool_call_duration_seconds", "tool" => name.to_string())
        .record(duration_secs);
}

/// Record LLM token usage.
pub fn record_llm_tokens(direction: &str, count: u32) {
    counter!("octos_llm_tokens_total", "direction" => direction.to_string())
        .increment(u64::from(count));
}

/// Decorator that records Prometheus metrics for progress events,
/// then delegates to an inner reporter.
pub struct MetricsReporter {
    inner: Arc<dyn octos_agent::ProgressReporter>,
}

impl MetricsReporter {
    pub fn new(inner: Arc<dyn octos_agent::ProgressReporter>) -> Self {
        Self { inner }
    }
}

impl octos_agent::ProgressReporter for MetricsReporter {
    fn report(&self, event: octos_agent::ProgressEvent) {
        match &event {
            octos_agent::ProgressEvent::ToolCompleted {
                name,
                success,
                duration,
                ..
            } => {
                record_tool_call(name, *success, duration.as_secs_f64());
            }
            octos_agent::ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                ..
            } => {
                record_llm_tokens("input", *session_input_tokens);
                record_llm_tokens("output", *session_output_tokens);
            }
            _ => {}
        }
        self.inner.report(event);
    }
}
