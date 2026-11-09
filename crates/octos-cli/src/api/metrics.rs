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
    /// Profiles surfaced by the process manager in a non-running state
    /// (configuration errors). Kept out of `running_gateways` and the
    /// api-port counters so a broken config does not read as a live gateway.
    pub configuration_error_gateways: usize,
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
    /// Whether the underlying process is actually running. Non-running rows
    /// (e.g. `ProcessState::ConfigurationError`) are surfaced as their own
    /// state instead of being counted as running gateways.
    pub running: bool,
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
    let mut configuration_error_gateways = 0;
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
            if source.running {
                running_gateways += 1;
                if source.api_port.is_some() {
                    gateways_with_api_port += 1;
                } else {
                    gateways_missing_api_port += 1;
                }
                if source.scrape_status == "failed" {
                    scrape_failures += 1;
                }
            } else {
                // Configuration-error profiles are surfaced by all_statuses()
                // without a live process; count them as their own state.
                configuration_error_gateways += 1;
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
            configuration_error_gateways,
            gateways_with_api_port,
            gateways_missing_api_port,
            scrape_failures,
            sources_observed,
            sources_with_metrics,
            sources_without_metrics,
            partial: collection_is_partial(
                gateways_missing_api_port,
                scrape_failures,
                sources_without_metrics,
            ),
        },
        totals,
        breakdowns,
        sources: source_rows,
    }
}

fn empty_collection() -> OperatorSummaryCollection {
    OperatorSummaryCollection {
        running_gateways: 0,
        configuration_error_gateways: 0,
        gateways_with_api_port: 0,
        gateways_missing_api_port: 0,
        scrape_failures: 0,
        sources_observed: 0,
        sources_with_metrics: 0,
        sources_without_metrics: 0,
        partial: false,
    }
}

fn collection_is_partial(
    gateways_missing_api_port: usize,
    scrape_failures: usize,
    sources_without_metrics: usize,
) -> bool {
    gateways_missing_api_port > 0 || scrape_failures > 0 || sources_without_metrics > 0
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
        (
            "mcp_server_calls".to_string(),
            total_for_metric(samples, "octos_mcp_server_call_total"),
        ),
        (
            "sub_agent_dispatches".to_string(),
            total_for_metric(samples, "octos_sub_agent_dispatch_total"),
        ),
        (
            "swarm_dispatches".to_string(),
            total_for_metric(samples, "octos_swarm_dispatch_total"),
        ),
        (
            "cost_attributions".to_string(),
            total_for_metric(samples, "octos_cost_attribution_total"),
        ),
        (
            "delegations".to_string(),
            total_for_metric(samples, "octos_delegation_total"),
        ),
        (
            "routing_decisions".to_string(),
            total_for_metric(samples, "octos_routing_decision_total"),
        ),
        (
            "credential_rotations".to_string(),
            total_for_metric(samples, "octos_llm_credential_rotation_total"),
        ),
        (
            "compaction_preservation_violations".to_string(),
            total_for_metric(samples, "octos_compaction_preservation_violations_total"),
        ),
        (
            "loop_errors".to_string(),
            total_for_metric(samples, octos_agent::OCTOS_LOOP_ERROR_TOTAL),
        ),
        (
            "loop_retries".to_string(),
            total_for_metric(samples, octos_agent::OCTOS_LOOP_RETRY_TOTAL),
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
        (
            "mcp_server_calls".to_string(),
            breakdown(samples, "octos_mcp_server_call_total", &["tool", "outcome"]),
        ),
        (
            "sub_agent_dispatches".to_string(),
            breakdown(
                samples,
                "octos_sub_agent_dispatch_total",
                &["backend", "outcome"],
            ),
        ),
        (
            "swarm_dispatches".to_string(),
            breakdown(
                samples,
                "octos_swarm_dispatch_total",
                &["topology", "outcome"],
            ),
        ),
        (
            "cost_attributions".to_string(),
            breakdown(
                samples,
                "octos_cost_attribution_total",
                &["model", "outcome"],
            ),
        ),
        (
            "delegations".to_string(),
            breakdown(samples, "octos_delegation_total", &["depth", "outcome"]),
        ),
        (
            "routing_decisions".to_string(),
            breakdown(samples, "octos_routing_decision_total", &["tier", "lane"]),
        ),
        (
            "credential_rotations".to_string(),
            breakdown(
                samples,
                "octos_llm_credential_rotation_total",
                &["reason", "strategy"],
            ),
        ),
        (
            "compaction_preservation_violations".to_string(),
            breakdown(
                samples,
                "octos_compaction_preservation_violations_total",
                &["phase"],
            ),
        ),
        (
            "loop_errors".to_string(),
            breakdown(
                samples,
                octos_agent::OCTOS_LOOP_ERROR_TOTAL,
                &["variant", "recovery"],
            ),
        ),
        (
            "loop_retries".to_string(),
            breakdown(
                samples,
                octos_agent::OCTOS_LOOP_RETRY_TOTAL,
                &["variant", "decision"],
            ),
        ),
    ])
}

/// Operator-facing cost rollup exposed alongside the summary. Consumed
/// by the admin dashboard to show a per-contract spend breakdown from
/// the ledger. Construction is isolated behind a pure function so it
/// remains unit-testable without touching redb.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OperatorContractCostSummary {
    pub contract_id: String,
    pub dispatch_count: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
}

/// Convert [`octos_agent::ContractCostRollup`] records into the
/// serialized dashboard shape. The conversion is deliberately
/// one-way so the API layer stays decoupled from the ledger crate's
/// internal types.
pub fn operator_contract_cost_summary(
    rollups: &[octos_agent::ContractCostRollup],
) -> Vec<OperatorContractCostSummary> {
    rollups
        .iter()
        .map(|rollup| OperatorContractCostSummary {
            contract_id: rollup.contract_id.clone(),
            dispatch_count: rollup.dispatch_count,
            tokens_in: rollup.tokens_in,
            tokens_out: rollup.tokens_out,
            cost_usd: rollup.cost_usd,
        })
        .collect()
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

// ── Operator harness task aggregation ────────────────────────────────
//
// The operator harness dashboard aggregates background task state across all
// running gateways and derives stale/missing-artifact signals from the same
// backend truth that `/api/sessions/:id/tasks` exposes.

/// Default staleness threshold: a task that has not updated in this many
/// seconds while still in a non-terminal lifecycle state is surfaced as
/// stale/zombie.
pub const DEFAULT_TASK_STALE_SECS: i64 = 300;

/// Input for `build_operator_tasks_response` — one task snapshot per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorTaskInput {
    pub profile_id: String,
    pub session_id: String,
    pub task_id: String,
    pub tool_name: String,
    pub lifecycle_state: String,
    pub runtime_state: Option<String>,
    pub workflow_kind: Option<String>,
    pub current_phase: Option<String>,
    pub child_session_key: Option<String>,
    pub child_terminal_state: Option<String>,
    pub child_join_state: Option<String>,
    pub child_failure_action: Option<String>,
    pub output_files: Vec<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Summary fields derived from a task row — computed once, surfaced in the UI
/// and used for filter counts.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorTaskDerived {
    pub stale: bool,
    pub missing_artifact: bool,
    pub validator_failed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorTaskView {
    pub profile_id: String,
    pub session_id: String,
    pub task_id: String,
    pub tool_name: String,
    pub lifecycle_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_terminal_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_join_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_failure_action: Option<String>,
    pub output_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub derived: OperatorTaskDerived,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorTaskSource {
    pub profile_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
    pub session_count: usize,
    pub task_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OperatorTasksResponse {
    pub generated_at: String,
    pub stale_threshold_secs: i64,
    pub tasks: Vec<OperatorTaskView>,
    /// Counts grouped by `lifecycle_state` — the dashboard row count per state
    /// must match these values.
    pub totals_by_lifecycle: BTreeMap<String, u64>,
    pub stale_count: u64,
    pub missing_artifact_count: u64,
    pub validator_failed_count: u64,
    pub sources: Vec<OperatorTaskSource>,
    /// True if at least one source returned an error — UI should show a banner.
    pub partial: bool,
}

/// Derive stale/missing-artifact flags for a task row. Exposed as a pure
/// function so the derivation rule is unit-tested independently from the
/// aggregation plumbing.
pub fn derive_operator_task_flags(
    input: &OperatorTaskInput,
    now: chrono::DateTime<chrono::Utc>,
    stale_threshold_secs: i64,
) -> OperatorTaskDerived {
    let is_terminal_ok = input.lifecycle_state == "ready";
    let is_failed = input.lifecycle_state == "failed";
    let is_active = matches!(
        input.lifecycle_state.as_str(),
        "queued" | "running" | "verifying"
    );

    let stale = if is_active {
        match input
            .updated_at
            .as_deref()
            .and_then(parse_rfc3339_timestamp)
        {
            Some(timestamp) => {
                let age = (now - timestamp).num_seconds();
                age >= stale_threshold_secs
            }
            None => false,
        }
    } else {
        false
    };

    // "Missing artifact" = reached terminal Ready but produced no output_files,
    // or failed after the runtime selected child outputs but none were kept.
    let missing_artifact = is_terminal_ok && input.output_files.is_empty();

    // Validator-failure = child terminal state indicates a terminal failure and
    // the task is either failed or currently verifying an unsuccessful child.
    let validator_failed = is_failed
        && matches!(
            input.child_terminal_state.as_deref(),
            Some("terminal_failed") | Some("retryable_failed")
        );

    OperatorTaskDerived {
        stale,
        missing_artifact,
        validator_failed,
    }
}

fn parse_rfc3339_timestamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Build the operator tasks response from a list of per-task inputs and a set
/// of per-profile source records.
pub fn build_operator_tasks_response(
    inputs: Vec<OperatorTaskInput>,
    sources: Vec<OperatorTaskSource>,
    now: chrono::DateTime<chrono::Utc>,
    stale_threshold_secs: i64,
) -> OperatorTasksResponse {
    let mut totals_by_lifecycle: BTreeMap<String, u64> =
        ["queued", "running", "verifying", "ready", "failed"]
            .iter()
            .map(|state| ((*state).to_string(), 0))
            .collect();

    let mut stale_count: u64 = 0;
    let mut missing_artifact_count: u64 = 0;
    let mut validator_failed_count: u64 = 0;

    let mut tasks: Vec<OperatorTaskView> = inputs
        .into_iter()
        .map(|input| {
            let derived = derive_operator_task_flags(&input, now, stale_threshold_secs);
            *totals_by_lifecycle
                .entry(input.lifecycle_state.clone())
                .or_insert(0) += 1;
            if derived.stale {
                stale_count += 1;
            }
            if derived.missing_artifact {
                missing_artifact_count += 1;
            }
            if derived.validator_failed {
                validator_failed_count += 1;
            }
            OperatorTaskView {
                profile_id: input.profile_id,
                session_id: input.session_id,
                task_id: input.task_id,
                tool_name: input.tool_name,
                lifecycle_state: input.lifecycle_state,
                runtime_state: input.runtime_state,
                workflow_kind: input.workflow_kind,
                current_phase: input.current_phase,
                child_session_key: input.child_session_key,
                child_terminal_state: input.child_terminal_state,
                child_join_state: input.child_join_state,
                child_failure_action: input.child_failure_action,
                output_files: input.output_files,
                error: input.error,
                started_at: input.started_at,
                updated_at: input.updated_at,
                completed_at: input.completed_at,
                derived,
            }
        })
        .collect();

    tasks.sort_by(|left, right| {
        lifecycle_priority(&left.lifecycle_state)
            .cmp(&lifecycle_priority(&right.lifecycle_state))
            .then_with(|| {
                right
                    .updated_at
                    .as_deref()
                    .unwrap_or("")
                    .cmp(left.updated_at.as_deref().unwrap_or(""))
            })
            .then_with(|| left.task_id.cmp(&right.task_id))
    });

    let partial = sources.iter().any(|source| source.status != "ok");

    OperatorTasksResponse {
        generated_at: now.to_rfc3339(),
        stale_threshold_secs,
        tasks,
        totals_by_lifecycle,
        stale_count,
        missing_artifact_count,
        validator_failed_count,
        sources,
        partial,
    }
}

fn lifecycle_priority(state: &str) -> u8 {
    // Render actionable states first. `failed` is most urgent; `ready` is
    // surfaced last because it is a success terminus.
    match state {
        "failed" => 0,
        "verifying" => 1,
        "running" => 2,
        "queued" => 3,
        "ready" => 4,
        _ => 5,
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

/// Record a content-classified smart routing decision (M6.6).
///
/// Increments `octos_routing_decision_total{tier, lane}`. `lane` is optional —
/// the classifier emits decisions before lane selection, so callers either
/// pass the lane they ultimately picked or `None` to record as `"unset"`.
pub fn record_routing_decision(tier: &str, lane: Option<&str>) {
    let lane_label = lane.unwrap_or("unset").to_string();
    counter!(
        "octos_routing_decision_total",
        "tier" => tier.to_string(),
        "lane" => lane_label,
    )
    .increment(1);
}

/// Decorator that records Prometheus metrics for progress events,
/// then delegates to an inner reporter.
pub struct MetricsReporter {
    inner: Arc<dyn octos_agent::ProgressReporter>,
    /// Turn-cumulative counters at the last `CostUpdate` this reporter
    /// metered. `octos_llm_tokens_total` increments by the DELTA between
    /// successive events so responses that emit no cost update of their
    /// own (mid-turn tool iterations, the verifier) are still counted by
    /// the next event that does fire. The reporter is built per turn, so
    /// the counters start at zero with the turn; a smaller-than-last
    /// value (a reporter reused across turns) is treated as a reset.
    last_turn_input_tokens: std::sync::atomic::AtomicU32,
    last_turn_output_tokens: std::sync::atomic::AtomicU32,
}

impl MetricsReporter {
    pub fn new(inner: Arc<dyn octos_agent::ProgressReporter>) -> Self {
        Self {
            inner,
            last_turn_input_tokens: std::sync::atomic::AtomicU32::new(0),
            last_turn_output_tokens: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Delta since the last metered value, treating a decrease as a
    /// turn reset (the new value counts in full).
    fn meter_delta(last: &std::sync::atomic::AtomicU32, now: u32) -> u32 {
        let prev = last.swap(now, std::sync::atomic::Ordering::AcqRel);
        if now >= prev { now - prev } else { now }
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
                turn_input_tokens,
                turn_output_tokens,
                ..
            } => {
                // Meter the delta of the TURN-cumulative counters (see
                // the field docs on `MetricsReporter`). The old form
                // added the cumulative session values on every event —
                // multiply-counting earlier responses and overflowing
                // the u32 boundary once those went session-wide u64.
                record_llm_tokens(
                    "input",
                    Self::meter_delta(&self.last_turn_input_tokens, *turn_input_tokens),
                );
                record_llm_tokens(
                    "output",
                    Self::meter_delta(&self.last_turn_output_tokens, *turn_output_tokens),
                );
            }
            _ => {}
        }
        self.inner.report(event);
    }

    /// Issue #960 fix: forward the inner reporter's bound `thread_id` so
    /// `agent/execution.rs`'s spawn_only intercept (which calls
    /// `reporter.thread_id()` to capture `bg_originating_thread_id` and
    /// `bg_originating_client_message_id`) sees the value the standalone
    /// turn bound at construction time
    /// (`BoundedChannelReporter::with_thread_id(turn_id.0.to_string())`)
    /// instead of falling through to the default `None`. Without this
    /// override the `MetricsReporter` decorator silently strips the
    /// thread binding and the `turn/spawn_complete` envelope's
    /// `response_to_client_message_id` lands as `None`, leaving the SPA
    /// reducer's thread-map lookup to miss and the new bubble to drop.
    fn thread_id(&self) -> Option<&str> {
        self.inner.thread_id()
    }
}
