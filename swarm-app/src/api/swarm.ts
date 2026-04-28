/**
 * Typed client for the M7.6 swarm dispatch dashboard.
 *
 * Consumes:
 *   - POST   /api/swarm/dispatch
 *   - GET    /api/swarm/dispatches
 *   - GET    /api/swarm/dispatches/{id}
 *   - POST   /api/swarm/dispatches/{id}/review
 *   - GET    /api/cost/attributions/{dispatch_id}
 *
 * And subscribes to the existing `/api/events` SSE stream for live
 * progress updates (no new channel, per invariant 3).
 */

// ── Primitive types (mirrored from octos-swarm so validation runs client-side) ──

export type TopologyKind = 'parallel' | 'sequential' | 'pipeline' | 'fanout'

export interface ContractSpec {
  contract_id: string
  tool_name: string
  task: Record<string, unknown>
  label?: string | null
}

export interface FanoutPattern {
  seed: ContractSpec
  variants: string[]
}

export type SwarmTopology =
  | { kind: 'parallel'; max_concurrency: number }
  | { kind: 'sequential' }
  | { kind: 'pipeline' }
  | { kind: 'fanout'; pattern: FanoutPattern; max_concurrency: number }

export interface SwarmBudget {
  max_contracts?: number | null
  max_retry_rounds?: number | null
  /** Optional per-dispatch USD ceiling surfaced in the UI — backend ignores */
  per_dispatch_usd?: number | null
  /** Optional per-contract USD ceiling surfaced in the UI — backend ignores */
  per_contract_usd?: number | null
}

export interface SwarmContextSpec {
  session_id: string
  task_id: string
  workflow?: string | null
  phase?: string | null
}

// ── Request / response shapes ──

export interface DispatchRequest {
  schema_version: 1
  dispatch_id: string
  contract_id: string
  contracts: ContractSpec[]
  topology: SwarmTopology
  budget?: SwarmBudget
  context?: SwarmContextSpec
}

export interface DispatchResponse {
  dispatch_id: string
  outcome: 'success' | 'partial' | 'failed' | 'aborted' | string
  total_subtasks: number
  completed_subtasks: number
}

export interface DispatchIndexRow {
  dispatch_id: string
  contract_id: string
  topology: TopologyKind | string
  outcome: 'success' | 'partial' | 'failed' | 'aborted' | string
  total_subtasks: number
  completed_subtasks: number
  retry_rounds_used: number
  created_at: string
  total_cost_usd?: number | null
  review_accepted?: boolean | null
}

export interface DispatchesResponse {
  dispatches: DispatchIndexRow[]
}

export type SubtaskLifecycleStatus =
  | 'completed'
  | 'retryable_failed'
  | 'terminal_failed'

export interface SubtaskView {
  contract_id: string
  label?: string | null
  status: SubtaskLifecycleStatus | string
  attempts: number
  last_dispatch_outcome: string
  output: string
  error?: string | null
}

export interface ValidatorView {
  name: string
  passed: boolean
  message?: string | null
}

export interface CostAttributionView {
  attribution_id: string
  contract_id: string
  model: string
  tokens_in: number
  tokens_out: number
  cost_usd: number
  outcome: string
  timestamp: string
}

export interface DispatchDetail {
  schema_version: number
  dispatch_id: string
  contract_id: string
  topology: string
  outcome: string
  total_subtasks: number
  completed_subtasks: number
  retry_rounds_used: number
  finalized: boolean
  subtasks: SubtaskView[]
  validator_evidence: ValidatorView[]
  cost_attributions: CostAttributionView[]
  total_cost_usd: number
  review_accepted?: boolean | null
  review_reviewer?: string | null
  review_notes?: string | null
}

export interface CostAttributionsResponse {
  dispatch_id: string
  attributions: CostAttributionView[]
  total_cost_usd: number
  total_tokens_in: number
  total_tokens_out: number
  count: number
}

export interface ReviewRequest {
  schema_version: 1
  accepted: boolean
  reviewer: string
  notes?: string | null
}

export interface ReviewResponse {
  dispatch_id: string
  accepted: boolean
  reviewer: string
  schema_version: number
}

// ── Template contracts for the Author tab ──

export const CONTRACT_TEMPLATES: Record<string, DispatchRequest> = {
  'parallel-n': {
    schema_version: 1,
    dispatch_id: 'parallel-1',
    contract_id: 'parallel-demo',
    contracts: [
      {
        contract_id: 'sub-1',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'write a haiku about the ocean' },
        label: 'haiku-ocean',
      },
      {
        contract_id: 'sub-2',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'write a haiku about mountains' },
        label: 'haiku-mountain',
      },
    ],
    topology: { kind: 'parallel', max_concurrency: 2 },
    budget: { max_retry_rounds: 2 },
    context: {
      session_id: 'api:swarm-dashboard',
      task_id: 'task-1',
      workflow: 'swarm',
      phase: 'dispatch',
    },
  },
  sequential: {
    schema_version: 1,
    dispatch_id: 'sequential-1',
    contract_id: 'sequential-demo',
    contracts: [
      {
        contract_id: 'step-1',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'draft an outline' },
      },
      {
        contract_id: 'step-2',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'expand the outline into prose' },
      },
    ],
    topology: { kind: 'sequential' },
    budget: {},
  },
  pipeline: {
    schema_version: 1,
    dispatch_id: 'pipeline-1',
    contract_id: 'pipeline-demo',
    contracts: [
      {
        contract_id: 'gather',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'gather facts about rust async' },
      },
      {
        contract_id: 'summarize',
        tool_name: 'claude_code/run_task',
        task: { prompt: 'summarize the facts into a brief' },
      },
    ],
    topology: { kind: 'pipeline' },
    budget: {},
  },
  fanout: {
    schema_version: 1,
    dispatch_id: 'fanout-1',
    contract_id: 'fanout-demo',
    contracts: [],
    topology: {
      kind: 'fanout',
      max_concurrency: 3,
      pattern: {
        seed: {
          contract_id: 'seed',
          tool_name: 'claude_code/run_task',
          task: { prompt: 'write a short note' },
          label: 'note',
        },
        variants: ['a', 'b', 'c'],
      },
    },
    budget: {},
  },
}

// ── Validation ──

export interface ValidationIssue {
  field: string
  message: string
}

/**
 * Schema-validate a contract spec before dispatch. Mirrors the backend's
 * `validate_dispatch_request` so malformed contracts are rejected
 * client-side before a POST.
 */
export function validateDispatchRequest(
  req: Partial<DispatchRequest>,
): ValidationIssue[] {
  const issues: ValidationIssue[] = []
  if (!req.dispatch_id || req.dispatch_id.trim() === '') {
    issues.push({ field: 'dispatch_id', message: 'dispatch_id is required' })
  }
  if (!req.contract_id || req.contract_id.trim() === '') {
    issues.push({ field: 'contract_id', message: 'contract_id is required' })
  }
  if (!req.topology) {
    issues.push({ field: 'topology', message: 'topology is required' })
    return issues
  }
  const topology = req.topology
  if (topology.kind === 'fanout') {
    if (!topology.pattern || !topology.pattern.variants?.length) {
      issues.push({
        field: 'topology.pattern.variants',
        message: 'fanout pattern must declare at least one variant',
      })
    }
  } else if (!req.contracts || req.contracts.length === 0) {
    issues.push({
      field: 'contracts',
      message: 'contracts list cannot be empty',
    })
  }
  if (req.contracts) {
    req.contracts.forEach((c, i) => {
      if (!c.contract_id) {
        issues.push({
          field: `contracts[${i}].contract_id`,
          message: 'contract_id is required',
        })
      }
      if (!c.tool_name) {
        issues.push({
          field: `contracts[${i}].tool_name`,
          message: 'tool_name is required',
        })
      }
    })
  }
  if (topology.kind === 'parallel' && (!topology.max_concurrency || topology.max_concurrency <= 0)) {
    issues.push({
      field: 'topology.max_concurrency',
      message: 'max_concurrency must be > 0',
    })
  }
  if (req.budget) {
    if (
      typeof req.budget.max_retry_rounds === 'number' &&
      req.budget.max_retry_rounds > 3
    ) {
      issues.push({
        field: 'budget.max_retry_rounds',
        message: 'max_retry_rounds may not exceed 3 (MAX_RETRY_ROUNDS)',
      })
    }
    if (
      typeof req.budget.max_contracts === 'number' &&
      req.budget.max_contracts > 128
    ) {
      issues.push({
        field: 'budget.max_contracts',
        message:
          'max_contracts may not exceed 128 (MAX_CONTRACTS_PER_DISPATCH)',
      })
    }
    if (
      typeof req.budget.per_dispatch_usd === 'number' &&
      req.budget.per_dispatch_usd < 0
    ) {
      issues.push({
        field: 'budget.per_dispatch_usd',
        message: 'per-dispatch USD cap must be non-negative',
      })
    }
  }
  return issues
}

/**
 * Parse a JSON or TOML-ish contract body. Today only JSON is supported;
 * the editor accepts TOML textually but the parse function rejects
 * non-JSON bodies with a descriptive error so the user can switch.
 */
export function parseContractBody(body: string): {
  parsed?: DispatchRequest
  error?: string
} {
  const trimmed = body.trim()
  if (!trimmed) {
    return { error: 'contract body is empty' }
  }
  try {
    const parsed = JSON.parse(trimmed) as DispatchRequest
    return { parsed }
  } catch (e: unknown) {
    return { error: e instanceof Error ? e.message : String(e) }
  }
}

// ── HTTP client ──

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

async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: { ...authHeaders(), ...(init?.headers ?? {}) },
  })
  if (!res.ok) {
    const body = await res.text().catch(() => '')
    throw new Error(body || `HTTP ${res.status}`)
  }
  return res.json() as Promise<T>
}

export const swarmApi = {
  dispatch: (req: DispatchRequest) =>
    fetchJson<DispatchResponse>('/api/swarm/dispatch', {
      method: 'POST',
      body: JSON.stringify(req),
    }),
  list: () => fetchJson<DispatchesResponse>('/api/swarm/dispatches'),
  detail: (id: string) =>
    fetchJson<DispatchDetail>(
      `/api/swarm/dispatches/${encodeURIComponent(id)}`,
    ),
  costAttributions: (id: string) =>
    fetchJson<CostAttributionsResponse>(
      `/api/cost/attributions/${encodeURIComponent(id)}`,
    ),
  review: (id: string, req: ReviewRequest) =>
    fetchJson<ReviewResponse>(
      `/api/swarm/dispatches/${encodeURIComponent(id)}/review`,
      {
        method: 'POST',
        body: JSON.stringify(req),
      },
    ),
}

// ── SSE subscription helper for the Live tab ──

/**
 * Subscribe to the existing `/api/chat/stream` SSE broadcaster. New
 * event types (swarm_dispatch, swarm_review_decision) flow through the
 * same stream by design (invariant 3 — no new channel).
 *
 * Returns the EventSource so the caller can close() on teardown.
 */
export function subscribeToEvents(
  onEvent: (data: unknown) => void,
  onError?: (e: Event) => void,
): EventSource {
  // The legacy broadcaster is at /api/chat/stream; auth flows through
  // the ?token=... query param so SSE works without Authorization
  // headers (which EventSource cannot attach).
  const token =
    localStorage.getItem('octos_session_token') ||
    localStorage.getItem('octos_auth_token') ||
    ''
  const url = token
    ? `/api/chat/stream?token=${encodeURIComponent(token)}`
    : '/api/chat/stream'
  const es = new EventSource(url)
  es.onmessage = (msg: MessageEvent) => {
    try {
      const parsed = JSON.parse(msg.data)
      onEvent(parsed)
    } catch {
      // Ignore non-JSON frames (keep-alive comments, etc.)
    }
  }
  if (onError) {
    es.onerror = onError
  }
  return es
}

/**
 * Topology label → stable short string for grouping in the Live view.
 */
export function topologyLabel(topology: string): string {
  switch (topology) {
    case 'parallel':
      return 'Parallel'
    case 'sequential':
      return 'Sequential'
    case 'pipeline':
      return 'Pipeline'
    case 'fanout':
      return 'Fan-out'
    default:
      return topology
  }
}

export function outcomeToneClass(outcome: string): string {
  switch (outcome) {
    case 'success':
      return 'text-green-300'
    case 'partial':
      return 'text-yellow-300'
    case 'failed':
      return 'text-red-300'
    case 'aborted':
      return 'text-red-300'
    default:
      return 'text-gray-300'
  }
}
