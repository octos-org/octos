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

/**
 * M6.1 — structured harness error taxonomy.
 *
 * Counter name is `octos_loop_error_total` and rolls up to the
 * `loop_errors` key in `OperatorSummaryResponse.totals` and
 * `OperatorSummaryResponse.breakdowns`. `variant` names match
 * `HarnessError::variant_name()` in Rust (snake_case identifiers such as
 * `rate_limited`, `context_overflow`, `delegate_depth_exceeded`).
 * `recovery` names match `RecoveryHint::as_str()`
 * (`backoff_retry`, `switch_provider`, `compact_context`, `fail_fast`,
 * `bug`).
 */
export interface HarnessErrorBreakdownRow extends OperatorSummaryBreakdownRow {
  variant: string
  recovery: string
}

export function harnessErrorRows(
  summary: OperatorSummaryResponse | null,
): HarnessErrorBreakdownRow[] {
  const rows = summary?.breakdowns.loop_errors ?? []
  return rows as HarnessErrorBreakdownRow[]
}

export function harnessErrorTotal(summary: OperatorSummaryResponse | null): number {
  return summary?.totals.loop_errors ?? 0
}

/**
 * M6.2 — loop retry bucket decisions.
 *
 * Counter `octos_loop_retry_total{variant, decision}` — `variant` is the
 * `HarnessError::variant_name()` (plus the synthetic `shell_spiral` bucket),
 * `decision` is `LoopDecision::as_str()` (one of
 * `continue | rotate_and_retry | compact_and_retry | escalate | exhausted |
 * grace`). Every observation is bounded by `LoopRetryLimits` — an
 * `exhausted` row means that bucket ran past its hard limit.
 */
export interface LoopRetryBreakdownRow extends OperatorSummaryBreakdownRow {
  variant: string
  decision: string
}

export function loopRetryRows(
  summary: OperatorSummaryResponse | null,
): LoopRetryBreakdownRow[] {
  const rows = summary?.breakdowns.loop_retries ?? []
  return rows as LoopRetryBreakdownRow[]
}

export function loopRetryTotal(summary: OperatorSummaryResponse | null): number {
  return summary?.totals.loop_retries ?? 0
}

/**
 * Per-variant aggregation suitable for the "bucket state" panel: sum every
 * decision for a given variant, then surface how many of those observations
 * ended in `exhausted`. Variants whose `exhausted_share` exceeds 0.5 should
 * be flagged in the UI (invariant from #495).
 */
export interface LoopRetryBucketSummary {
  variant: string
  total: number
  exhausted: number
  escalate: number
  continue_count: number
  rotate: number
  compact: number
  grace: number
  exhausted_share: number
}

export function loopRetryBuckets(
  summary: OperatorSummaryResponse | null,
): LoopRetryBucketSummary[] {
  const acc = new Map<string, LoopRetryBucketSummary>()
  for (const row of loopRetryRows(summary)) {
    const variant = row.variant ?? 'unknown'
    const count = Number(row.count ?? 0)
    const bucket = acc.get(variant) ?? {
      variant,
      total: 0,
      exhausted: 0,
      escalate: 0,
      continue_count: 0,
      rotate: 0,
      compact: 0,
      grace: 0,
      exhausted_share: 0,
    }
    bucket.total += count
    switch (row.decision) {
      case 'exhausted':
        bucket.exhausted += count
        break
      case 'escalate':
        bucket.escalate += count
        break
      case 'continue':
        bucket.continue_count += count
        break
      case 'rotate_and_retry':
        bucket.rotate += count
        break
      case 'compact_and_retry':
        bucket.compact += count
        break
      case 'grace':
        bucket.grace += count
        break
      default:
        break
    }
    acc.set(variant, bucket)
  }
  const out = Array.from(acc.values()).map((b) => ({
    ...b,
    exhausted_share: b.total > 0 ? b.exhausted / b.total : 0,
  }))
  out.sort((a, b) => b.exhausted_share - a.exhausted_share || b.total - a.total)
  return out
}

/**
 * M6.3 — compaction preservation violations.
 *
 * Counter `octos_compaction_preservation_violations_total{phase}` counts
 * cases where the compaction policy dropped or mutated messages that the
 * workspace contract required to be preserved verbatim. A non-zero total is
 * always a bug signal.
 */
export interface CompactionViolationRow extends OperatorSummaryBreakdownRow {
  phase: string
}

export function compactionViolationRows(
  summary: OperatorSummaryResponse | null,
): CompactionViolationRow[] {
  const rows = summary?.breakdowns.compaction_preservation_violations ?? []
  return rows as CompactionViolationRow[]
}

export function compactionViolationTotal(
  summary: OperatorSummaryResponse | null,
): number {
  return summary?.totals.compaction_preservation_violations ?? 0
}

/**
 * M6.5 — credential pool rotations.
 *
 * Counter `octos_llm_credential_rotation_total{reason, strategy}`. The
 * dashboard surfaces the reason/strategy mix and the total count, which is a
 * rough proxy for how often the pool cycles credentials (auth failures,
 * cooldowns, manual releases).
 */
export interface CredentialRotationRow extends OperatorSummaryBreakdownRow {
  reason: string
  strategy: string
}

export function credentialRotationRows(
  summary: OperatorSummaryResponse | null,
): CredentialRotationRow[] {
  const rows = summary?.breakdowns.credential_rotations ?? []
  return rows as CredentialRotationRow[]
}

export function credentialRotationTotal(
  summary: OperatorSummaryResponse | null,
): number {
  return summary?.totals.credential_rotations ?? 0
}

/**
 * Rotation counts broken down purely by reason. Useful for the
 * "active / cooldown" readout on the credential pool card.
 */
export interface CredentialReasonSummary {
  reason: string
  count: number
}

export function credentialRotationsByReason(
  summary: OperatorSummaryResponse | null,
): CredentialReasonSummary[] {
  const acc = new Map<string, number>()
  for (const row of credentialRotationRows(summary)) {
    const reason = row.reason ?? 'unknown'
    acc.set(reason, (acc.get(reason) ?? 0) + Number(row.count ?? 0))
  }
  return Array.from(acc.entries())
    .map(([reason, count]) => ({ reason, count }))
    .sort((a, b) => b.count - a.count)
}

/**
 * M6.6 — content-classified smart routing decisions.
 *
 * Counter `octos_routing_decision_total{tier, lane}`. `tier` is
 * `cheap | strong`, `lane` is an optional pool-aware hint set by M6.5.
 * A `cheap_share` close to 1.0 means the router offloaded most chat turns
 * to the cheap tier; anything near 0 flags the router as falling back to
 * the strong tier too aggressively.
 */
export interface RoutingDecisionRow extends OperatorSummaryBreakdownRow {
  tier: string
  lane: string
}

export function routingDecisionRows(
  summary: OperatorSummaryResponse | null,
): RoutingDecisionRow[] {
  const rows = summary?.breakdowns.routing_decisions ?? []
  return rows as RoutingDecisionRow[]
}

export function routingDecisionTotal(
  summary: OperatorSummaryResponse | null,
): number {
  return summary?.totals.routing_decisions ?? 0
}

export interface RoutingDecisionSummary {
  cheap: number
  strong: number
  other: number
  total: number
  cheap_share: number
}

export function routingDecisionSummary(
  summary: OperatorSummaryResponse | null,
): RoutingDecisionSummary {
  let cheap = 0
  let strong = 0
  let other = 0
  for (const row of routingDecisionRows(summary)) {
    const count = Number(row.count ?? 0)
    if (row.tier === 'cheap') cheap += count
    else if (row.tier === 'strong') strong += count
    else other += count
  }
  const total = cheap + strong + other
  return {
    cheap,
    strong,
    other,
    total,
    cheap_share: total > 0 ? cheap / total : 0,
  }
}

/**
 * True when at least one signal on the task row matches the "loop warning"
 * filter (stale runtime, missing artifact, validator failure, or a harness
 * error recorded on `task.error`). Used for the
 * "show only sessions with loop warnings" filter on the dashboard.
 */
export function taskHasLoopWarning(task: HarnessTaskView): boolean {
  if (task.derived.stale) return true
  if (task.derived.missing_artifact) return true
  if (task.derived.validator_failed) return true
  if (task.error && task.error.trim().length > 0) return true
  return false
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
