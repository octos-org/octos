import { useCallback, useEffect, useState } from 'react'
import {
  outcomeToneClass,
  swarmApi,
  topologyLabel,
  type CostAttributionsResponse,
  type DispatchDetail,
  type DispatchIndexRow,
} from '../../api/swarm'

type ReviewOutcome = 'accepted' | 'rejected' | null

/**
 * Review gate panel: lists completed dispatches, shows contract
 * invariant evidence (validator outcomes from M4.3) + the live cost
 * attribution breakdown (M7.4 ledger), and posts a
 * `SwarmReviewDecision` typed event.
 */
export default function ReviewGate() {
  const [dispatches, setDispatches] = useState<DispatchIndexRow[]>([])
  const [activeId, setActiveId] = useState<string | null>(null)
  const [detail, setDetail] = useState<DispatchDetail | null>(null)
  const [cost, setCost] = useState<CostAttributionsResponse | null>(null)
  const [notes, setNotes] = useState('')
  const [reviewer, setReviewer] = useState('')
  const [status, setStatus] = useState<ReviewOutcome>(null)
  const [error, setError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)

  const refresh = useCallback(async () => {
    try {
      const resp = await swarmApi.list()
      setDispatches(resp.dispatches)
      setError(null)
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  useEffect(() => {
    if (!activeId) {
      setDetail(null)
      setCost(null)
      return
    }
    let cancelled = false
    ;(async () => {
      try {
        const [d, c] = await Promise.all([
          swarmApi.detail(activeId),
          swarmApi.costAttributions(activeId),
        ])
        if (cancelled) return
        setDetail(d)
        setCost(c)
        setStatus(
          d.review_accepted === true
            ? 'accepted'
            : d.review_accepted === false
              ? 'rejected'
              : null,
        )
      } catch (e: unknown) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [activeId])

  const submit = async (accepted: boolean) => {
    if (!activeId) return
    if (!reviewer.trim()) {
      setError('Reviewer is required')
      return
    }
    setSubmitting(true)
    setError(null)
    try {
      await swarmApi.review(activeId, {
        schema_version: 1,
        accepted,
        reviewer: reviewer.trim(),
        notes: notes.trim() || null,
      })
      setStatus(accepted ? 'accepted' : 'rejected')
      await refresh()
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div className="grid grid-cols-1 gap-4 md:grid-cols-[320px_1fr]" data-testid="swarm-review-gate">
      <aside className="rounded-xl border border-gray-700/50 bg-surface p-3">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-[11px] uppercase tracking-wider text-gray-500">
            Completed dispatches
          </span>
          <button
            type="button"
            onClick={() => void refresh()}
            className="text-[11px] text-gray-400 hover:text-gray-200"
          >
            Refresh
          </button>
        </div>
        {dispatches.length === 0 ? (
          <p
            data-testid="swarm-review-empty"
            className="px-2 py-3 text-xs text-gray-500"
          >
            No dispatches to review.
          </p>
        ) : (
          <ul className="space-y-1">
            {dispatches.map((d) => (
              <li key={d.dispatch_id}>
                <button
                  type="button"
                  onClick={() => setActiveId(d.dispatch_id)}
                  data-testid={`swarm-review-dispatch-${d.dispatch_id}`}
                  className={`w-full rounded-lg px-3 py-2 text-left text-xs transition-colors ${
                    activeId === d.dispatch_id
                      ? 'bg-accent/15 text-accent'
                      : 'text-gray-300 hover:bg-white/5'
                  }`}
                >
                  <div className="flex items-center justify-between">
                    <span className="font-mono truncate">{d.dispatch_id}</span>
                    <span
                      className={`text-[10px] uppercase tracking-wider ${outcomeToneClass(d.outcome)}`}
                    >
                      {d.outcome}
                    </span>
                  </div>
                  <div className="mt-0.5 text-[11px] text-gray-500">
                    {topologyLabel(d.topology)} · {d.completed_subtasks}/{d.total_subtasks}
                    {d.review_accepted === true && (
                      <span
                        className="ml-2 text-green-300"
                        data-testid={`swarm-review-accepted-${d.dispatch_id}`}
                      >
                        ✓ accepted
                      </span>
                    )}
                    {d.review_accepted === false && (
                      <span className="ml-2 text-red-300">✗ rejected</span>
                    )}
                  </div>
                </button>
              </li>
            ))}
          </ul>
        )}
      </aside>
      <section className="rounded-xl border border-gray-700/50 bg-surface p-4" data-testid="swarm-review-panel">
        {!activeId && (
          <p className="text-sm text-gray-500">
            Select a dispatch on the left to inspect its validator evidence and cost attribution.
          </p>
        )}
        {activeId && detail && (
          <div className="space-y-4">
            <header>
              <div className="flex items-baseline justify-between">
                <h3 className="font-mono text-sm text-gray-200">{detail.dispatch_id}</h3>
                <span
                  className={`text-xs font-semibold uppercase tracking-wider ${outcomeToneClass(detail.outcome)}`}
                >
                  {detail.outcome}
                </span>
              </div>
              <div className="mt-1 text-xs text-gray-500">
                {topologyLabel(detail.topology)} · {detail.contract_id} ·{' '}
                {detail.completed_subtasks}/{detail.total_subtasks} completed
              </div>
            </header>
            <div>
              <h4 className="mb-1.5 text-[11px] uppercase tracking-wider text-gray-500">
                Contract invariants (M4.3 validators)
              </h4>
              {(detail.validator_evidence ?? []).length === 0 ? (
                <p className="text-xs text-gray-600">
                  No validator evidence recorded for this dispatch.
                </p>
              ) : (
                <ul className="space-y-1 text-xs">
                  {(detail.validator_evidence ?? []).map((v) => (
                    <li
                      key={v.name}
                      data-testid={`swarm-review-validator-${v.name}`}
                      className="flex items-center justify-between rounded border border-gray-700/50 px-2.5 py-1"
                    >
                      <span className="font-mono text-gray-300">{v.name}</span>
                      <span
                        className={
                          v.passed ? 'text-green-300' : 'text-red-300'
                        }
                      >
                        {v.passed ? 'pass' : 'fail'}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <div>
              <h4 className="mb-1.5 text-[11px] uppercase tracking-wider text-gray-500">
                Cost attribution (M7.4 ledger)
              </h4>
              {cost && cost.attributions.length > 0 ? (
                <div className="space-y-1 text-xs">
                  <div className="flex items-center justify-between rounded border border-gray-700/50 bg-white/5 px-2.5 py-1">
                    <span className="text-gray-400">Total</span>
                    <span className="font-mono text-gray-200">
                      ${cost.total_cost_usd.toFixed(6)} · {cost.total_tokens_in} in · {cost.total_tokens_out} out
                    </span>
                  </div>
                  {cost.attributions.map((a) => (
                    <div
                      key={a.attribution_id}
                      data-testid={`swarm-review-attribution-${a.attribution_id}`}
                      className="flex items-center justify-between rounded border border-gray-700/30 px-2.5 py-1"
                    >
                      <span className="font-mono text-gray-400">{a.model}</span>
                      <span className="font-mono text-gray-300">
                        ${a.cost_usd.toFixed(6)} ({a.tokens_in}/{a.tokens_out})
                      </span>
                    </div>
                  ))}
                </div>
              ) : (
                <p className="text-xs text-gray-600">No cost attribution recorded yet.</p>
              )}
            </div>
            <div className="space-y-2">
              <h4 className="text-[11px] uppercase tracking-wider text-gray-500">
                Decision
              </h4>
              <label className="flex flex-col gap-1 text-xs text-gray-400">
                Reviewer
                <input
                  type="text"
                  value={reviewer}
                  onChange={(e) => setReviewer(e.target.value)}
                  data-testid="swarm-review-reviewer"
                  placeholder="you@example.com"
                  className="rounded-lg border border-gray-700/60 bg-surface px-3 py-1.5 text-sm text-gray-200 focus:border-accent/60 focus:outline-none"
                />
              </label>
              <label className="flex flex-col gap-1 text-xs text-gray-400">
                Notes (optional)
                <textarea
                  value={notes}
                  onChange={(e) => setNotes(e.target.value)}
                  data-testid="swarm-review-notes"
                  rows={2}
                  className="resize-y rounded-lg border border-gray-700/60 bg-surface px-3 py-1.5 text-sm text-gray-200 focus:border-accent/60 focus:outline-none"
                />
              </label>
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  onClick={() => void submit(true)}
                  disabled={submitting}
                  data-testid="swarm-review-accept"
                  className="rounded-lg bg-green-500/20 px-3 py-1.5 text-xs font-semibold text-green-200 hover:bg-green-500/30 disabled:opacity-60"
                >
                  Accept
                </button>
                <button
                  type="button"
                  onClick={() => void submit(false)}
                  disabled={submitting}
                  data-testid="swarm-review-reject"
                  className="rounded-lg bg-red-500/20 px-3 py-1.5 text-xs font-semibold text-red-200 hover:bg-red-500/30 disabled:opacity-60"
                >
                  Reject
                </button>
                {status === 'accepted' && (
                  <span
                    data-testid="swarm-review-accepted-state"
                    className="text-xs font-semibold text-green-300"
                  >
                    ✓ Accepted
                  </span>
                )}
                {status === 'rejected' && (
                  <span
                    data-testid="swarm-review-rejected-state"
                    className="text-xs font-semibold text-red-300"
                  >
                    ✗ Rejected
                  </span>
                )}
              </div>
              {error && (
                <div
                  data-testid="swarm-review-error"
                  className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300"
                >
                  {error}
                </div>
              )}
            </div>
          </div>
        )}
      </section>
    </div>
  )
}
