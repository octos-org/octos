//! Prometheus metrics endpoint and helpers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;

use axum::extract::State;
use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Serialize;
use serde_json::{Value, json};

use super::AppState;

static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
pub fn init_metrics() -> PrometheusHandle {
    METRICS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus recorder")
        })
        .clone()
}

/// GET /metrics -- render Prometheus text exposition format.
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    match state.metrics_handle {
        Some(ref handle) => handle.render(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMetricSample {
    name: String,
    labels: BTreeMap<String, String>,
    count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorSummaryCollection {
    pub running_gateways: usize,
    pub gateways_with_api_port: usize,
    pub gateways_missing_api_port: usize,
    pub scrape_failures: usize,
    pub sources_observed: usize,
    pub sources_with_metrics: usize,
    pub sources_without_metrics: usize,
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorSummarySource {
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub scrape_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrape_error: Option<String>,
    pub available: bool,
    pub sample_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<i64>,
    pub totals: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorSummary {
    pub available: bool,
    pub collection: OperatorSummaryCollection,
    pub totals: BTreeMap<String, u64>,
    pub breakdowns: BTreeMap<String, Vec<Value>>,
    pub sources: Vec<OperatorSummarySource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorSummarySourceInput {
    pub scope: String,
    pub profile_id: Option<String>,
    pub scrape_status: String,
    pub scrape_error: Option<String>,
    pub api_port: Option<u16>,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub uptime_secs: Option<i64>,
    pub metrics_text: Option<String>,
}

pub fn build_operator_summary(metrics_text: &str) -> OperatorSummary {
    let samples = parse_metric_samples(metrics_text);
    let (available, totals, breakdowns) = build_operator_summary_parts(&samples);

    OperatorSummary {
        available,
        collection: empty_collection(),
        totals,
        breakdowns,
        sources: Vec::new(),
    }
}

pub fn build_operator_summary_from_texts<I, S>(metrics_texts: I) -> OperatorSummary
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let samples = metrics_texts
        .into_iter()
        .flat_map(|text| parse_metric_samples(text.as_ref()))
        .collect::<Vec<_>>();
    let (available, totals, breakdowns) = build_operator_summary_parts(&samples);

    OperatorSummary {
        available,
        collection: empty_collection(),
        totals,
        breakdowns,
        sources: Vec::new(),
    }
}

pub fn build_operator_summary_from_sources<I>(sources: I) -> OperatorSummary
where
    I: IntoIterator<Item = OperatorSummarySourceInput>,
{
    let mut combined_samples = Vec::new();
    let mut source_rows = Vec::new();
    let mut running_gateways = 0;
    let mut gateways_with_api_port = 0;
    let mut gateways_missing_api_port = 0;
    let mut scrape_failures = 0;

    for source in sources {
        let samples = source
            .metrics_text
            .as_deref()
            .map(parse_metric_samples)
            .unwrap_or_default();
        let available = !samples.is_empty();
        let totals = build_totals(&samples);

        if source.scope == "gateway" {
            running_gateways += 1;
            if source.api_port.is_some() {
                gateways_with_api_port += 1;
            } else {
                gateways_missing_api_port += 1;
            }
            if source.scrape_status == "failed" {
                scrape_failures += 1;
            }
        }

        combined_samples.extend(samples.iter().cloned());
        source_rows.push(OperatorSummarySource {
            scope: source.scope,
            profile_id: source.profile_id,
            scrape_status: source.scrape_status,
            scrape_error: source.scrape_error,
            available,
            sample_count: samples.len(),
            api_port: source.api_port,
            pid: source.pid,
            started_at: source.started_at,
            uptime_secs: source.uptime_secs,
            totals,
        });
    }

    source_rows.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then_with(|| left.profile_id.cmp(&right.profile_id))
            .then_with(|| left.api_port.cmp(&right.api_port))
    });

    let (available, totals, breakdowns) = build_operator_summary_parts(&combined_samples);
    let sources_observed = source_rows.len();
    let sources_with_metrics = source_rows.iter().filter(|source| source.available).count();
    let sources_without_metrics = sources_observed.saturating_sub(sources_with_metrics);

    OperatorSummary {
        available,
        collection: OperatorSummaryCollection {
            running_gateways,
            gateways_with_api_port,
            gateways_missing_api_port,
            scrape_failures,
            sources_observed,
            sources_with_metrics,
            sources_without_metrics,
            partial: gateways_missing_api_port > 0 || scrape_failures > 0,
        },
        totals,
        breakdowns,
        sources: source_rows,
    }
}

fn empty_collection() -> OperatorSummaryCollection {
    OperatorSummaryCollection {
        running_gateways: 0,
        gateways_with_api_port: 0,
        gateways_missing_api_port: 0,
        scrape_failures: 0,
        sources_observed: 0,
        sources_with_metrics: 0,
        sources_without_metrics: 0,
        partial: false,
    }
}

fn build_operator_summary_parts(
    samples: &[ParsedMetricSample],
) -> (bool, BTreeMap<String, u64>, BTreeMap<String, Vec<Value>>) {
    let totals = build_totals(samples);
    let breakdowns = build_breakdowns(samples);
    (!samples.is_empty(), totals, breakdowns)
}

fn build_totals(samples: &[ParsedMetricSample]) -> BTreeMap<String, u64> {
    BTreeMap::from([
        (
            "retries".to_string(),
            total_for_metric(samples, "octos_retry_total"),
        ),
        (
            "timeouts".to_string(),
            total_for_metric(samples, "octos_timeout_total"),
        ),
        (
            "duplicate_suppressions".to_string(),
            total_for_metric(samples, "octos_result_duplicate_suppressed_total"),
        ),
        (
            "orphaned_child_sessions".to_string(),
            total_for_metric(samples, "octos_child_session_orphan_total"),
        ),
        (
            "workflow_phase_transitions".to_string(),
            total_for_metric(samples, "octos_workflow_phase_transition_total"),
        ),
        (
            "result_deliveries".to_string(),
            total_for_metric(samples, "octos_result_delivery_total"),
        ),
        (
            "session_replays".to_string(),
            total_for_metric(samples, "octos_session_replay_total"),
        ),
        (
            "session_persists".to_string(),
            total_for_metric(samples, "octos_session_persist_total"),
        ),
        (
            "session_rewrites".to_string(),
            total_for_metric(samples, "octos_session_rewrite_total"),
        ),
        (
            "child_session_lifecycle".to_string(),
            total_for_metric(samples, "octos_child_session_lifecycle_total"),
        ),
        (
            "realtime_heartbeat_beats".to_string(),
            total_for_metric(samples, "octos_realtime_heartbeat_beats_total"),
        ),
        (
            "realtime_heartbeat_stalls".to_string(),
            total_for_metric(samples, "octos_realtime_heartbeat_stalls_total"),
        ),
        (
            "workspace_validator_runs".to_string(),
            total_for_metric(samples, "octos_workspace_validator_total"),
        ),
        (
            "workspace_validator_required_failures".to_string(),
            total_for_metric(samples, "octos_workspace_validator_required_failed_total"),
        ),
        (
            "workspace_validator_optional_warnings".to_string(),
            total_for_metric(samples, "octos_workspace_validator_optional_warning_total"),
        ),
    ])
}

fn build_breakdowns(samples: &[ParsedMetricSample]) -> BTreeMap<String, Vec<Value>> {
    BTreeMap::from([
        (
            "retry_reasons".to_string(),
            breakdown(samples, "octos_retry_total", &["reason"]),
        ),
        (
            "timeout_reasons".to_string(),
            breakdown(samples, "octos_timeout_total", &["reason"]),
        ),
        (
            "duplicate_suppressions".to_string(),
            breakdown(
                samples,
                "octos_result_duplicate_suppressed_total",
                &["surface", "reason"],
            ),
        ),
        (
            "child_session_orphans".to_string(),
            breakdown(samples, "octos_child_session_orphan_total", &["reason"]),
        ),
        (
            "workflow_phase_transitions".to_string(),
            breakdown(
                samples,
                "octos_workflow_phase_transition_total",
                &["workflow_kind", "from_phase", "to_phase"],
            ),
        ),
        (
            "result_delivery".to_string(),
            breakdown(
                samples,
                "octos_result_delivery_total",
                &["path", "outcome", "kind"],
            ),
        ),
        (
            "session_replay".to_string(),
            breakdown(samples, "octos_session_replay_total", &["kind", "outcome"]),
        ),
        (
            "session_persist".to_string(),
            breakdown(samples, "octos_session_persist_total", &["outcome"]),
        ),
        (
            "session_rewrite".to_string(),
            breakdown(samples, "octos_session_rewrite_total", &["outcome"]),
        ),
        (
            "child_session_lifecycle".to_string(),
            breakdown(
                samples,
                "octos_child_session_lifecycle_total",
                &["kind", "outcome"],
            ),
        ),
        (
            "workspace_validator_runs".to_string(),
            breakdown(
                samples,
                "octos_workspace_validator_total",
                &["status", "phase", "kind", "required"],
            ),
        ),
    ])
}

fn total_for_metric(samples: &[ParsedMetricSample], metric: &str) -> u64 {
    samples
        .iter()
        .filter(|sample| sample.name == metric)
        .map(|sample| sample.count)
        .sum()
}

fn breakdown(samples: &[ParsedMetricSample], metric: &str, keys: &[&str]) -> Vec<Value> {
    let mut grouped: BTreeMap<String, (Vec<(String, String)>, u64)> = BTreeMap::new();

    for sample in samples.iter().filter(|sample| sample.name == metric) {
        let dims: Vec<(String, String)> = keys
            .iter()
            .map(|key| {
                (
                    (*key).to_string(),
                    sample
                        .labels
                        .get(*key)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string()),
                )
            })
            .collect();
        let group_key = dims
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("|");
        grouped
            .entry(group_key)
            .and_modify(|(_, count)| *count += sample.count)
            .or_insert((dims, sample.count));
    }

    let mut rows: Vec<Value> = grouped
        .into_values()
        .map(|(dims, count)| {
            let mut row = serde_json::Map::new();
            for (key, value) in dims {
                row.insert(key, Value::String(value));
            }
            row.insert("count".to_string(), json!(count));
            Value::Object(row)
        })
        .collect();

    rows.sort_by(|left, right| {
        let left_count = left.get("count").and_then(Value::as_u64).unwrap_or(0);
        let right_count = right.get("count").and_then(Value::as_u64).unwrap_or(0);
        right_count
            .cmp(&left_count)
            .then_with(|| left.to_string().cmp(&right.to_string()))
    });
    rows
}

fn parse_metric_samples(metrics_text: &str) -> Vec<ParsedMetricSample> {
    metrics_text.lines().filter_map(parse_metric_line).collect()
}

fn parse_metric_line(line: &str) -> Option<ParsedMetricSample> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let selector = parts.next()?;
    let value = parts.next()?.parse::<f64>().ok()?;

    let (name, labels) = match selector.split_once('{') {
        Some((name, rest)) => {
            let labels_raw = rest.strip_suffix('}')?;
            (name.to_string(), parse_labels(labels_raw))
        }
        None => (selector.to_string(), BTreeMap::new()),
    };

    Some(ParsedMetricSample {
        name,
        labels,
        count: value.max(0.0).round() as u64,
    })
}

fn parse_labels(raw: &str) -> BTreeMap<String, String> {
    split_label_pairs(raw)
        .into_iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((
                key.trim().to_string(),
                unescape_label_value(value.trim().trim_matches('"')),
            ))
        })
        .collect()
}

fn split_label_pairs(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in raw.chars() {
        match ch {
            '"' if !escaped => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            '\\' if !escaped => {
                escaped = true;
                current.push(ch);
                continue;
            }
            _ => current.push(ch),
        }
        escaped = false;
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

fn unescape_label_value(raw: &str) -> String {
    raw.replace("\\\"", "\"").replace("\\\\", "\\")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_summary_aggregates_key_runtime_counters() {
        let metrics = r#"
# TYPE octos_retry_total counter
octos_retry_total{reason="background_result_actor_closed"} 2
octos_retry_total{reason="background_result_actor_closed"} 1
octos_timeout_total{reason="background_result_ack_timeout"} 4
octos_result_duplicate_suppressed_total{surface="api_channel",reason="session_result_preferred_over_legacy_file_event"} 3
octos_child_session_orphan_total{reason="terminal_event_not_joined"} 1
octos_workflow_phase_transition_total{workflow_kind="research_podcast",from_phase="queued",to_phase="render"} 5
octos_result_delivery_total{path="session_result_event",outcome="committed_media_message",kind="session_result"} 7
octos_session_replay_total{kind="committed_session_result",outcome="replayed"} 8
octos_session_persist_total{outcome="ok"} 9
octos_session_rewrite_total{outcome="updated"} 10
octos_child_session_lifecycle_total{kind="completed",outcome="accepted"} 11
"#;

        let summary = build_operator_summary(metrics);

        assert!(summary.available);
        assert_eq!(summary.totals.get("retries"), Some(&3));
        assert_eq!(summary.totals.get("timeouts"), Some(&4));
        assert_eq!(summary.totals.get("duplicate_suppressions"), Some(&3));
        assert_eq!(summary.totals.get("orphaned_child_sessions"), Some(&1));
        assert_eq!(summary.totals.get("workflow_phase_transitions"), Some(&5));
        assert_eq!(summary.totals.get("result_deliveries"), Some(&7));

        let retry_rows = summary.breakdowns.get("retry_reasons").unwrap();
        assert_eq!(retry_rows.len(), 1);
        assert_eq!(retry_rows[0]["reason"], "background_result_actor_closed");
        assert_eq!(retry_rows[0]["count"], 3);

        let workflow_rows = summary
            .breakdowns
            .get("workflow_phase_transitions")
            .unwrap();
        assert_eq!(workflow_rows[0]["workflow_kind"], "research_podcast");
        assert_eq!(workflow_rows[0]["from_phase"], "queued");
        assert_eq!(workflow_rows[0]["to_phase"], "render");
        assert_eq!(workflow_rows[0]["count"], 5);
    }

    #[test]
    fn operator_summary_handles_empty_metrics_text() {
        let summary = build_operator_summary("");
        assert!(!summary.available);
        assert!(summary.totals.values().all(|count| *count == 0));
    }

    #[test]
    fn operator_summary_aggregates_across_sources() {
        let summary = build_operator_summary_from_texts([
            r#"
octos_session_persist_total{outcome="ok"} 2
octos_timeout_total{reason="session_turn"} 1
"#,
            r#"
octos_session_persist_total{outcome="ok"} 3
octos_session_replay_total{kind="committed_session_result",outcome="replayed"} 4
"#,
        ]);

        assert!(summary.available);
        assert_eq!(summary.totals.get("session_persists"), Some(&5));
        assert_eq!(summary.totals.get("timeouts"), Some(&1));
        assert_eq!(summary.totals.get("session_replays"), Some(&4));
    }

    #[test]
    fn operator_summary_tracks_source_collection_and_failures() {
        let summary = build_operator_summary_from_sources([
            OperatorSummarySourceInput {
                scope: "serve".into(),
                profile_id: None,
                scrape_status: "local".into(),
                scrape_error: None,
                api_port: None,
                pid: None,
                started_at: None,
                uptime_secs: None,
                metrics_text: Some("octos_timeout_total{reason=\"session_turn\"} 2".into()),
            },
            OperatorSummarySourceInput {
                scope: "gateway".into(),
                profile_id: Some("alpha".into()),
                scrape_status: "scraped".into(),
                scrape_error: None,
                api_port: Some(51001),
                pid: Some(4242),
                started_at: Some("2026-04-17T00:00:00Z".into()),
                uptime_secs: Some(120),
                metrics_text: Some(
                    "octos_retry_total{reason=\"background_result_ack_timeout\"} 3".into(),
                ),
            },
            OperatorSummarySourceInput {
                scope: "gateway".into(),
                profile_id: Some("beta".into()),
                scrape_status: "failed".into(),
                scrape_error: Some("http 503".into()),
                api_port: Some(51002),
                pid: Some(4343),
                started_at: None,
                uptime_secs: Some(45),
                metrics_text: None,
            },
            OperatorSummarySourceInput {
                scope: "gateway".into(),
                profile_id: Some("gamma".into()),
                scrape_status: "missing_api_port".into(),
                scrape_error: None,
                api_port: None,
                pid: Some(4444),
                started_at: None,
                uptime_secs: Some(30),
                metrics_text: None,
            },
        ]);

        assert!(summary.available);
        assert_eq!(summary.collection.running_gateways, 3);
        assert_eq!(summary.collection.gateways_with_api_port, 2);
        assert_eq!(summary.collection.gateways_missing_api_port, 1);
        assert_eq!(summary.collection.scrape_failures, 1);
        assert_eq!(summary.collection.sources_observed, 4);
        assert_eq!(summary.collection.sources_with_metrics, 2);
        assert_eq!(summary.collection.sources_without_metrics, 2);
        assert!(summary.collection.partial);
        assert_eq!(summary.totals.get("timeouts"), Some(&2));
        assert_eq!(summary.totals.get("retries"), Some(&3));

        let alpha = summary
            .sources
            .iter()
            .find(|source| source.profile_id.as_deref() == Some("alpha"))
            .unwrap();
        assert_eq!(alpha.scrape_status, "scraped");
        assert_eq!(alpha.sample_count, 1);
        assert_eq!(alpha.totals.get("retries"), Some(&3));

        let beta = summary
            .sources
            .iter()
            .find(|source| source.profile_id.as_deref() == Some("beta"))
            .unwrap();
        assert_eq!(beta.scrape_status, "failed");
        assert_eq!(beta.scrape_error.as_deref(), Some("http 503"));
        assert!(!beta.available);
    }
}
