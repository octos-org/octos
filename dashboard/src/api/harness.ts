/**
 * Typed client for the operator harness dashboard.
 *
 * Consumes existing backend endpoints:
 *   - GET /api/admin/operator/summary — aggregated Prometheus counters
 *   - GET /api/admin/operator/tasks   — aggregated harness background task rows
 *
 * The dashboard is a read-only view of backend truth — there is no UI-side
 * state machine. If a field needs to appear in the dashboard, the backend
 * must expose it (see crates/octos-cli/src/api/metrics.rs).
 */

export type HarnessLifecycleState =
  | 'queued'
  | 'running'
  | 'verifying'
  | 'ready'
  | 'failed'

export type HarnessRuntimeState =
  | 'spawned'
  | 'executing_tool'
  | 'resolving_outputs'
  | 'verifying_outputs'
  | 'delivering_outputs'
  | 'cleaning_up'
  | 'completed'
  | 'failed'

export type HarnessChildTerminalState =
  | 'completed'
  | 'retryable_failed'
  | 'terminal_failed'

export type HarnessChildJoinState = 'joined' | 'orphaned'

export type HarnessChildFailureAction = 'retry' | 'escalate'

export interface HarnessTaskDerived {
  stale: boolean
  missing_artifact: boolean
  validator_failed: boolean
}

export interface HarnessTaskView {
  profile_id: string
  session_id: string
  task_id: string
  tool_name: string
  lifecycle_state: HarnessLifecycleState | string
  runtime_state?: HarnessRuntimeState | string | null
  workflow_kind?: string | null
  current_phase?: string | null
  child_session_key?: string | null
  child_terminal_state?: HarnessChildTerminalState | string | null
  child_join_state?: HarnessChildJoinState | string | null
  child_failure_action?: HarnessChildFailureAction | string | null
  output_files: string[]
  error?: string | null
  started_at?: string | null
  updated_at?: string | null
  completed_at?: string | null
  derived: HarnessTaskDerived
}

export interface HarnessTaskSource {
  profile_id: string
  status: 'ok' | 'failed' | 'missing_api_port' | string
  error?: string | null
  api_port?: number | null
  session_count: number
  task_count: number
}

export interface HarnessTasksResponse {
  generated_at: string
  stale_threshold_secs: number
  tasks: HarnessTaskView[]
  totals_by_lifecycle: Partial<Record<HarnessLifecycleState, number>> & Record<string, number>
  stale_count: number
  missing_artifact_count: number
  validator_failed_count: number
  sources: HarnessTaskSource[]
  partial: boolean
}

export interface OperatorSummaryCollection {
  running_gateways: number
  gateways_with_api_port: number
  gateways_missing_api_port: number
  scrape_failures: number
  sources_observed: number
  sources_with_metrics: number
  sources_without_metrics: number
  partial: boolean
}

export interface OperatorSummarySource {
  scope: string
  profile_id?: string | null
  scrape_status: string
  scrape_error?: string | null
  available: boolean
  sample_count: number
  api_port?: number | null
  pid?: number | null
  started_at?: string | null
  uptime_secs?: number | null
  totals: Record<string, number>
}

export interface OperatorSummaryBreakdownRow {
  count: number
  [dimension: string]: string | number
}

export interface OperatorSummaryResponse {
  available: boolean
  collection: OperatorSummaryCollection
  totals: Record<string, number>
  breakdowns: Record<string, OperatorSummaryBreakdownRow[]>
  sources: OperatorSummarySource[]
}

function authHeaders(): HeadersInit {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' }
  const token =
    localStorage.getItem('octos_session_token') ||
    localStorage.getItem('octos_auth_token')
  if (token) {
    headers['Authorization'] = `Bearer ${token}`
  }
  return headers
}

async function fetchJson<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: authHeaders() })
  if (!res.ok) {
    const body = await res.text().catch(() => '')
    throw new Error(body || `HTTP ${res.status}`)
  }
  return res.json() as Promise<T>
}

export const harnessApi = {
  summary: () => fetchJson<OperatorSummaryResponse>('/api/admin/operator/summary'),
  tasks: () => fetchJson<HarnessTasksResponse>('/api/admin/operator/tasks'),
}
