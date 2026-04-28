import { useState } from 'react'
import ContractEditor from '../components/swarm/ContractEditor'
import DispatchForm from '../components/swarm/DispatchForm'
import LiveView from '../components/swarm/LiveView'
import ReviewGate from '../components/swarm/ReviewGate'
import {
  CONTRACT_TEMPLATES,
  type DispatchResponse,
} from '../api/swarm'

type TabKey = 'author' | 'dispatch' | 'live' | 'review'

const TAB_ORDER: { key: TabKey; label: string; hint: string }[] = [
  { key: 'author', label: 'Author', hint: 'Write and validate a contract spec' },
  { key: 'dispatch', label: 'Dispatch', hint: 'Submit the contract to the swarm primitive' },
  { key: 'live', label: 'Live', hint: 'Watch sub-agent lifecycle state in real time' },
  { key: 'review', label: 'Review', hint: 'Accept or reject completed dispatches' },
]

/**
 * Contract-authoring + swarm dispatch dashboard (M7.6).
 *
 * 4-tab layout:
 *   1. Author — contract spec editor, JSON schema-validated client-side
 *   2. Dispatch — POST form wiring to `/api/swarm/dispatch`
 *   3. Live — SSE-driven per-sub-agent progress grouped by dispatch_id
 *   4. Review — cost attribution + accept/reject gate
 *
 * Invariants honoured (per the acceptance contract):
 *   - UI never re-implements orchestration — it forwards to the backend
 *     which forwards to `octos_swarm::Swarm::dispatch`.
 *   - Live view subscribes to the existing `/api/events`-style stream;
 *     no new channel is opened.
 *   - Review submits as a typed `SwarmReviewDecision` event — never raw
 *     JSON — and the dashboard reads back the typed accepted flag.
 */
export default function SwarmPage() {
  const [active, setActive] = useState<TabKey>('author')
  const [contractBody, setContractBody] = useState<string>(
    JSON.stringify(CONTRACT_TEMPLATES['parallel-n'], null, 2),
  )
  const [lastDispatch, setLastDispatch] = useState<DispatchResponse | null>(null)

  return (
    <div className="space-y-4" data-testid="swarm-page">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold text-gray-100">Swarm orchestrator</h1>
          <p className="mt-1 max-w-2xl text-sm text-gray-500">
            Author contracts, dispatch swarms, watch live progress, and gate
            decisions through a typed review event. Consumes the M7.5 primitive
            via the octos-cli REST surface.
          </p>
        </div>
        {lastDispatch && (
          <div
            data-testid="swarm-last-dispatch"
            className="rounded-lg border border-green-500/30 bg-green-500/5 px-3 py-1.5 text-xs text-green-200"
          >
            Last dispatch: <span className="font-mono">{lastDispatch.dispatch_id}</span> ·{' '}
            {lastDispatch.completed_subtasks}/{lastDispatch.total_subtasks} ·{' '}
            {lastDispatch.outcome}
          </div>
        )}
      </header>
      <nav
        role="tablist"
        aria-label="Swarm dashboard sections"
        className="flex flex-wrap gap-2 border-b border-gray-700/50 pb-2"
        data-testid="swarm-tablist"
      >
        {TAB_ORDER.map((tab) => (
          <button
            key={tab.key}
            type="button"
            role="tab"
            aria-selected={active === tab.key}
            data-testid={`swarm-tab-${tab.key}`}
            onClick={() => setActive(tab.key)}
            className={`rounded-lg px-3 py-1.5 text-sm transition-colors ${
              active === tab.key
                ? 'bg-accent/15 text-accent'
                : 'text-gray-400 hover:bg-white/5 hover:text-gray-200'
            }`}
            title={tab.hint}
          >
            {tab.label}
          </button>
        ))}
      </nav>
      <section role="tabpanel">
        {active === 'author' && (
          <div data-testid="swarm-panel-author">
            <ContractEditor value={contractBody} onChange={setContractBody} />
          </div>
        )}
        {active === 'dispatch' && (
          <div data-testid="swarm-panel-dispatch">
            <DispatchForm
              contractBody={contractBody}
              onDispatched={(resp) => {
                setLastDispatch(resp)
                setActive('live')
              }}
            />
          </div>
        )}
        {active === 'live' && (
          <div data-testid="swarm-panel-live">
            <LiveView />
          </div>
        )}
        {active === 'review' && (
          <div data-testid="swarm-panel-review">
            <ReviewGate />
          </div>
        )}
      </section>
    </div>
  )
}
