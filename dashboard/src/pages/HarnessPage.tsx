import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import HarnessTaskTable, { LIFECYCLE_ORDER } from '../components/HarnessTaskTable'
import {
  harnessApi,
  type HarnessLifecycleState,
  type HarnessTasksResponse,
  type OperatorSummaryResponse,
} from '../api/harness'

const POLL_INTERVAL_MS = 5000

type LifecycleFilter = HarnessLifecycleState | 'all'

interface DerivedFilter {
  stale: boolean
  missingArtifact: boolean
  validatorFailed: boolean
}

function formatTimestamp(iso: string | null | undefined): string {
  if (!iso) return '—'
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return iso
  return new Date(t).toLocaleTimeString()
}

function CountCard({
  label,
  value,
  tone = 'default',
  testId,
  highlight = false,
  onClick,
  active = false,
}: {
  label: string
  value: number | string
  tone?: 'default' | 'danger' | 'warn' | 'ok' | 'accent'
  testId?: string
  highlight?: boolean
  onClick?: () => void
  active?: boolean
}) {
  const toneStyles: Record<string, string> = {
    default: 'text-gray-200',
    danger: 'text-red-300',
    warn: 'text-yellow-300',
    ok: 'text-green-300',
    accent: 'text-accent',
  }
  const activeRing = active ? 'ring-2 ring-accent/60 border-accent/40' : ''
  const highlightRing = highlight && !active ? 'border-yellow-500/30' : ''
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!onClick}
      data-testid={testId}
      data-active={active ? 'true' : 'false'}
      className={`text-left bg-surface border border-gray-700/50 rounded-xl px-4 py-3 transition hover:bg-white/[0.03] disabled:cursor-default disabled:hover:bg-surface ${highlightRing} ${activeRing}`}
    >
      <div className="text-[11px] uppercase tracking-wider text-gray-500">{label}</div>
      <div className={`text-2xl font-semibold tabular-nums ${toneStyles[tone]}`}>{value}</div>
    </button>
  )
}

export default function HarnessPage() {
  const [summary, setSummary] = useState<OperatorSummaryResponse | null>(null)
  const [tasksResp, setTasksResp] = useState<HarnessTasksResponse | null>(null)
  const [tasksError, setTasksError] = useState<string | null>(null)
  const [summaryError, setSummaryError] = useState<string | null>(null)
  const [live, setLive] = useState(true)
  const [now, setNow] = useState(() => Date.now())
  const [lifecycleFilter, setLifecycleFilter] = useState<LifecycleFilter>('all')
  const [derivedFilter, setDerivedFilter] = useState<DerivedFilter>({
    stale: false,
    missingArtifact: false,
    validatorFailed: false,
  })
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const fetchOnce = useCallback(async () => {
    try {
      const s = await harnessApi.summary()
      setSummary(s)
      setSummaryError(null)
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      setSummaryError(msg)
    }
    try {
      const t = await harnessApi.tasks()
      setTasksResp(t)
      setTasksError(null)
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      setTasksError(msg)
    }
    setNow(Date.now())
  }, [])

  useEffect(() => {
    fetchOnce()
  }, [fetchOnce])

  useEffect(() => {
    if (!live) {
      if (timerRef.current) {
        clearInterval(timerRef.current)
        timerRef.current = null
      }
      return
    }
    timerRef.current = setInterval(fetchOnce, POLL_INTERVAL_MS)
    return () => {
      if (timerRef.current) {
        clearInterval(timerRef.current)
        timerRef.current = null
      }
    }
  }, [live, fetchOnce])

  const filteredTasks = useMemo(() => {
    const all = tasksResp?.tasks ?? []
    return all.filter((task) => {
      if (lifecycleFilter !== 'all' && task.lifecycle_state !== lifecycleFilter) {
        return false
      }
      if (derivedFilter.stale && !task.derived.stale) return false
      if (derivedFilter.missingArtifact && !task.derived.missing_artifact) return false
      if (derivedFilter.validatorFailed && !task.derived.validator_failed) return false
      return true
    })
  }, [tasksResp, lifecycleFilter, derivedFilter])

  const totals = tasksResp?.totals_by_lifecycle ?? {}
  const summaryTotals = summary?.totals ?? {}
  const breakdowns = summary?.breakdowns ?? {}

  return (
    <div className="space-y-6" data-testid="harness-page">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-white">Operator Harness</h1>
          <p className="text-sm text-gray-500">
            Lifecycle, phase, artifact, and failure state across every running gateway.
            {tasksResp && (
              <span data-testid="harness-generated-at" className="ml-2 text-gray-600">
                snapshot at {formatTimestamp(tasksResp.generated_at)}
              </span>
            )}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => {
              void fetchOnce()
            }}
            className="text-xs px-3 py-1.5 rounded-lg border border-gray-700 text-gray-300 hover:bg-white/5"
            data-testid="harness-refresh"
          >
            Refresh
          </button>
          <button
            type="button"
            onClick={() => setLive((v) => !v)}
            className="flex items-center gap-2 px-3 py-1.5 text-xs font-medium rounded-lg border border-gray-700 hover:bg-white/5 transition"
            data-testid="harness-live-toggle"
          >
            <span
              className={`inline-block w-2 h-2 rounded-full ${live ? 'bg-green-500 animate-pulse' : 'bg-gray-600'}`}
            />
            {live ? 'Live' : 'Paused'}
          </button>
        </div>
      </div>

      {tasksResp?.partial && (
        <div
          data-testid="harness-partial-banner"
          className="text-sm text-yellow-300 bg-yellow-500/5 border border-yellow-500/30 rounded-lg px-3 py-2"
        >
          Partial collection: one or more gateways did not return task data. See the Sources
          panel below for per-profile status.
        </div>
      )}

      {/* Lifecycle state counts — row count per state matches totals_by_lifecycle */}
      <div
        className="grid grid-cols-2 md:grid-cols-5 gap-3"
        data-testid="lifecycle-counts"
      >
        <CountCard
          label="All"
          value={tasksResp?.tasks.length ?? 0}
          testId="count-all"
          onClick={() => setLifecycleFilter('all')}
          active={lifecycleFilter === 'all'}
        />
        {LIFECYCLE_ORDER.map((state) => (
          <CountCard
            key={state}
            label={state}
            value={totals[state] ?? 0}
            testId={`count-${state}`}
            tone={
              state === 'failed'
                ? 'danger'
                : state === 'verifying'
                  ? 'warn'
                  : state === 'ready'
                    ? 'ok'
                    : 'default'
            }
            onClick={() => setLifecycleFilter(state)}
            active={lifecycleFilter === state}
          />
        ))}
      </div>

      {/* Derived signal counts */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-3" data-testid="derived-counts">
        <CountCard
          label="Stale / zombie"
          value={tasksResp?.stale_count ?? 0}
          testId="count-stale"
          tone="warn"
          highlight={(tasksResp?.stale_count ?? 0) > 0}
          onClick={() =>
            setDerivedFilter((d) => ({ ...d, stale: !d.stale }))
          }
          active={derivedFilter.stale}
        />
        <CountCard
          label="Missing artifact"
          value={tasksResp?.missing_artifact_count ?? 0}
          testId="count-missing-artifact"
          tone={(tasksResp?.missing_artifact_count ?? 0) > 0 ? 'danger' : 'ok'}
          highlight={(tasksResp?.missing_artifact_count ?? 0) > 0}
          onClick={() =>
            setDerivedFilter((d) => ({ ...d, missingArtifact: !d.missingArtifact }))
          }
          active={derivedFilter.missingArtifact}
        />
        <CountCard
          label="Validator failed"
          value={tasksResp?.validator_failed_count ?? 0}
          testId="count-validator-failed"
          tone={(tasksResp?.validator_failed_count ?? 0) > 0 ? 'danger' : 'ok'}
          highlight={(tasksResp?.validator_failed_count ?? 0) > 0}
          onClick={() =>
            setDerivedFilter((d) => ({ ...d, validatorFailed: !d.validatorFailed }))
          }
          active={derivedFilter.validatorFailed}
        />
      </div>

      {tasksError && (
        <div
          data-testid="harness-tasks-error"
          className="text-sm text-red-300 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2"
        >
          Failed to load tasks: {tasksError}
        </div>
      )}

      <HarnessTaskTable tasks={filteredTasks} now={now} />

      {/* Per-gateway sources */}
      {tasksResp && tasksResp.sources.length > 0 && (
        <div className="bg-surface border border-gray-700/50 rounded-xl" data-testid="harness-sources">
          <div className="px-5 py-3 border-b border-gray-700/30">
            <h2 className="text-sm font-semibold text-gray-300">Gateway Sources</h2>
            <p className="text-[11px] text-gray-500">
              Per-profile collection status. Any non-ok source means the row counts above are
              partial.
            </p>
          </div>
          <table className="w-full text-sm">
            <thead>
              <tr className="text-xs text-gray-500 border-b border-gray-700/30">
                <th className="text-left py-2 px-5 font-medium">Profile</th>
                <th className="text-left py-2 px-3 font-medium">Status</th>
                <th className="text-right py-2 px-3 font-medium">API Port</th>
                <th className="text-right py-2 px-3 font-medium">Sessions</th>
                <th className="text-right py-2 px-3 font-medium">Tasks</th>
                <th className="text-left py-2 px-5 font-medium">Error</th>
              </tr>
            </thead>
            <tbody>
              {tasksResp.sources.map((src) => (
                <tr
                  key={src.profile_id}
                  data-testid="harness-source-row"
                  data-profile-id={src.profile_id}
                  data-status={src.status}
                  className="border-b border-gray-700/20 last:border-0"
                >
                  <td className="py-2 px-5 font-mono text-gray-300">{src.profile_id}</td>
                  <td className="py-2 px-3">
                    <span
                      className={`px-2 py-0.5 rounded-full text-[11px] font-mono uppercase tracking-wider ${
                        src.status === 'ok'
                          ? 'bg-green-500/15 text-green-300 border border-green-500/30'
                          : src.status === 'failed'
                            ? 'bg-red-500/15 text-red-300 border border-red-500/30'
                            : 'bg-yellow-500/15 text-yellow-300 border border-yellow-500/30'
                      }`}
                    >
                      {src.status}
                    </span>
                  </td>
                  <td className="py-2 px-3 text-right font-mono text-gray-400">
                    {src.api_port ?? '—'}
                  </td>
                  <td className="py-2 px-3 text-right font-mono text-gray-400">
                    {src.session_count}
                  </td>
                  <td className="py-2 px-3 text-right font-mono text-gray-400">
                    {src.task_count}
                  </td>
                  <td className="py-2 px-5 text-gray-500 text-[12px] truncate max-w-[240px]">
                    {src.error || '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* Operator summary (retry/timeout/etc) */}
      {summary && (
        <div className="bg-surface border border-gray-700/50 rounded-xl" data-testid="harness-summary">
          <div className="px-5 py-3 border-b border-gray-700/30 flex items-center justify-between">
            <div>
              <h2 className="text-sm font-semibold text-gray-300">Runtime Counters</h2>
              <p className="text-[11px] text-gray-500">
                Aggregated from `/api/admin/operator/summary` (same truth the CLI operator
                summary reads).
              </p>
            </div>
            {summary.collection.partial && (
              <span className="text-[11px] text-yellow-300">
                partial: {summary.collection.gateways_missing_api_port} missing,{' '}
                {summary.collection.scrape_failures} failed
              </span>
            )}
          </div>
          <div className="p-5 grid grid-cols-2 md:grid-cols-4 gap-3">
            {Object.entries(summaryTotals).map(([metric, value]) => (
              <div
                key={metric}
                data-testid={`summary-total-${metric}`}
                className="border border-gray-700/40 rounded-lg px-3 py-2"
              >
                <div className="text-[11px] uppercase tracking-wider text-gray-500">
                  {metric.replace(/_/g, ' ')}
                </div>
                <div className="text-lg font-semibold text-gray-200 tabular-nums">{value}</div>
              </div>
            ))}
          </div>
          {breakdowns.workflow_phase_transitions &&
            breakdowns.workflow_phase_transitions.length > 0 && (
              <div className="px-5 pb-5">
                <h3 className="text-[12px] uppercase tracking-wider text-gray-500 mb-2">
                  Workflow phase transitions
                </h3>
                <table className="w-full text-[12px]">
                  <thead>
                    <tr className="text-[10px] text-gray-600 border-b border-gray-700/30">
                      <th className="text-left py-1 font-medium">workflow_kind</th>
                      <th className="text-left py-1 font-medium">from_phase</th>
                      <th className="text-left py-1 font-medium">to_phase</th>
                      <th className="text-right py-1 font-medium">count</th>
                    </tr>
                  </thead>
                  <tbody>
                    {breakdowns.workflow_phase_transitions.slice(0, 10).map((row, i) => (
                      <tr key={i} className="border-b border-gray-700/15 last:border-0">
                        <td className="py-1 text-gray-400 font-mono">
                          {String(row.workflow_kind ?? '—')}
                        </td>
                        <td className="py-1 text-gray-500 font-mono">
                          {String(row.from_phase ?? '—')}
                        </td>
                        <td className="py-1 text-gray-500 font-mono">
                          {String(row.to_phase ?? '—')}
                        </td>
                        <td className="py-1 text-right font-mono text-gray-300">
                          {String(row.count ?? 0)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
        </div>
      )}

      {summaryError && (
        <div
          data-testid="harness-summary-error"
          className="text-sm text-red-300 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2"
        >
          Failed to load operator summary: {summaryError}
        </div>
      )}
    </div>
  )
}
