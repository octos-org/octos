import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  outcomeToneClass,
  subscribeToEvents,
  swarmApi,
  topologyLabel,
  type DispatchDetail,
  type DispatchIndexRow,
  type SubtaskView,
} from '../../api/swarm'

interface LiveRow {
  dispatch_id: string
  contract_id: string
  topology: string
  outcome: string
  total_subtasks: number
  completed_subtasks: number
  retry_round?: number | null
  message?: string | null
  last_event_at: string
  subtasks?: SubtaskView[]
}

/**
 * Live-view panel. Subscribes to `/api/chat/stream` SSE — the existing
 * harness event stream — and re-groups incoming events by dispatch_id.
 * Newly-observed dispatches trigger a detail fetch so per-subtask state
 * lands inline. Falls back to polling `/api/swarm/dispatches` every 10s.
 */
export default function LiveView() {
  const [rows, setRows] = useState<Record<string, LiveRow>>({})
  const [error, setError] = useState<string | null>(null)
  const [connected, setConnected] = useState(false)

  const upsert = useCallback((patch: LiveRow) => {
    setRows((prev) => ({ ...prev, [patch.dispatch_id]: { ...prev[patch.dispatch_id], ...patch } }))
  }, [])

  const handleEvent = useCallback(
    (data: unknown) => {
      if (!data || typeof data !== 'object') return
      const obj = data as Record<string, unknown>
      const kind = String(obj.kind ?? '')
      if (kind === 'swarm_dispatch' && typeof obj.dispatch_id === 'string') {
        upsert({
          dispatch_id: obj.dispatch_id,
          contract_id: String(obj.contract_id ?? obj.dispatch_id),
          topology: String(obj.topology ?? 'unknown'),
          outcome: String(obj.outcome ?? 'unknown'),
          total_subtasks: Number(obj.total_subtasks ?? 0),
          completed_subtasks: Number(obj.completed_subtasks ?? 0),
          retry_round: obj.retry_round as number | undefined,
          message: (obj.message as string | null) ?? null,
          last_event_at: new Date().toISOString(),
        })
      }
      if (kind === 'swarm_review_decision' && typeof obj.dispatch_id === 'string') {
        // Mark the row as reviewed so the UI shows the badge.
        setRows((prev) => {
          const cur = prev[obj.dispatch_id as string]
          if (!cur) return prev
          return {
            ...prev,
            [obj.dispatch_id as string]: {
              ...cur,
              message: `Reviewed: ${obj.accepted ? 'accepted' : 'rejected'}`,
              last_event_at: new Date().toISOString(),
            },
          }
        })
      }
    },
    [upsert],
  )

  // SSE subscription — invariant 3: uses the existing /api/events style
  // stream (exposed at /api/chat/stream in the octos-cli backend).
  useEffect(() => {
    let es: EventSource | null = null
    try {
      es = subscribeToEvents(handleEvent, () => setConnected(false))
      es.onopen = () => setConnected(true)
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e))
    }
    return () => {
      if (es) es.close()
    }
  }, [handleEvent])

  // Initial + periodic fallback fetch so the view survives an SSE drop.
  useEffect(() => {
    let cancelled = false
    const load = async () => {
      try {
        const list = await swarmApi.list()
        if (cancelled) return
        list.dispatches.forEach((row) => {
          upsert(fromIndexRow(row))
        })
        setError(null)
      } catch (e: unknown) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      }
    }
    load()
    const timer = setInterval(load, 10_000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [upsert])

  // When a new dispatch surfaces, fetch its detail once so sub-agent
  // lifecycle state is populated. Guarded so we only fetch per-id once.
  const detailsFetched = useMemo(() => new Set<string>(), [])
  useEffect(() => {
    Object.keys(rows).forEach(async (id) => {
      if (detailsFetched.has(id)) return
      detailsFetched.add(id)
      try {
        const detail: DispatchDetail = await swarmApi.detail(id)
        upsert({
          dispatch_id: detail.dispatch_id,
          contract_id: detail.contract_id,
          topology: detail.topology,
          outcome: detail.outcome,
          total_subtasks: detail.total_subtasks,
          completed_subtasks: detail.completed_subtasks,
          last_event_at: new Date().toISOString(),
          subtasks: detail.subtasks,
        })
      } catch {
        // swallow — the periodic fallback will retry
      }
    })
  }, [rows, detailsFetched, upsert])

  const orderedRows = useMemo(
    () =>
      Object.values(rows).sort((a, b) => {
        return (b.last_event_at ?? '').localeCompare(a.last_event_at ?? '')
      }),
    [rows],
  )

  return (
    <div className="flex flex-col gap-3" data-testid="swarm-live-view">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2 text-xs text-gray-400">
          <span
            className={`inline-block h-2 w-2 rounded-full ${
              connected ? 'bg-green-400' : 'bg-gray-500'
            }`}
          />
          <span>{connected ? 'Connected to /api/chat/stream' : 'Awaiting SSE…'}</span>
        </div>
        <div className="text-[11px] text-gray-500">
          {orderedRows.length} dispatch{orderedRows.length === 1 ? '' : 'es'}
        </div>
      </div>
      {error && (
        <div
          data-testid="swarm-live-error"
          className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300"
        >
          {error}
        </div>
      )}
      {orderedRows.length === 0 ? (
        <div
          data-testid="swarm-live-empty"
          className="rounded-xl border border-gray-700/50 bg-surface px-4 py-6 text-center text-sm text-gray-500"
        >
          No swarm dispatches yet. Author a contract on the Author tab and submit via Dispatch.
        </div>
      ) : (
        orderedRows.map((row) => <LiveRowCard key={row.dispatch_id} row={row} />)
      )}
    </div>
  )
}

function fromIndexRow(row: DispatchIndexRow): LiveRow {
  return {
    dispatch_id: row.dispatch_id,
    contract_id: row.contract_id,
    topology: row.topology,
    outcome: row.outcome,
    total_subtasks: row.total_subtasks,
    completed_subtasks: row.completed_subtasks,
    retry_round: row.retry_rounds_used,
    message: null,
    last_event_at: row.created_at,
  }
}

function LiveRowCard({ row }: { row: LiveRow }) {
  return (
    <div
      data-testid={`swarm-live-row-${row.dispatch_id}`}
      className="rounded-xl border border-gray-700/50 bg-surface p-4"
    >
      <div className="flex items-start justify-between gap-4">
        <div>
          <div className="flex items-baseline gap-2">
            <span className="font-mono text-sm text-gray-200">{row.dispatch_id}</span>
            <span className="text-xs text-gray-500">
              {topologyLabel(row.topology)} · {row.contract_id}
            </span>
          </div>
          <div className="mt-1 text-xs text-gray-400">
            {row.completed_subtasks}/{row.total_subtasks} sub-agents completed
            {typeof row.retry_round === 'number' && row.retry_round > 0 ? (
              <span> · retry {row.retry_round}</span>
            ) : null}
          </div>
          {row.message && (
            <div className="mt-2 text-xs text-gray-500">{row.message}</div>
          )}
        </div>
        <div className={`text-xs font-semibold uppercase tracking-wider ${outcomeToneClass(row.outcome)}`}>
          {row.outcome}
        </div>
      </div>
      {row.subtasks && row.subtasks.length > 0 && (
        <ul className="mt-3 space-y-1.5 border-t border-gray-700/50 pt-3">
          {row.subtasks.map((s) => (
            <li
              key={s.contract_id}
              data-testid={`swarm-live-subtask-${s.contract_id}`}
              className="flex items-center justify-between text-xs"
            >
              <span className="font-mono text-gray-300">
                {s.label ?? s.contract_id}
              </span>
              <span className="flex items-center gap-3">
                <span className="text-gray-500">
                  {s.attempts} attempt{s.attempts === 1 ? '' : 's'}
                </span>
                <SubtaskBadge status={s.status} />
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function SubtaskBadge({ status }: { status: string }) {
  const cls =
    status === 'completed'
      ? 'text-green-300 border-green-500/40'
      : status === 'terminal_failed'
        ? 'text-red-300 border-red-500/40'
        : 'text-yellow-300 border-yellow-500/40'
  return (
    <span
      className={`rounded border px-1.5 py-0.5 text-[10px] uppercase tracking-wider ${cls}`}
    >
      {status.replace(/_/g, ' ')}
    </span>
  )
}
