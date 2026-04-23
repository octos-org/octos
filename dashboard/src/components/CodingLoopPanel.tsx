/**
 * Coding-loop health panels surfacing the M6 harness signals.
 *
 * The dashboard is read-only — every number here comes from the
 * `OperatorSummaryResponse` exposed by
 * `/api/admin/operator/summary`. If a field should appear here, the backend
 * has to expose it via `octos-cli/src/api/metrics.rs`. See `api/harness.ts`
 * for the typed helpers that shape the Prometheus breakdown rows.
 */
import {
  compactionViolationRows,
  compactionViolationTotal,
  credentialRotationRows,
  credentialRotationTotal,
  credentialRotationsByReason,
  loopRetryBuckets,
  loopRetryTotal,
  routingDecisionRows,
  routingDecisionSummary,
  type OperatorSummaryResponse,
} from '../api/harness'

function SectionCard({
  title,
  description,
  testId,
  children,
  empty,
}: {
  title: string
  description: string
  testId: string
  children: React.ReactNode
  empty?: boolean
}) {
  return (
    <div
      className="bg-surface border border-gray-700/50 rounded-xl"
      data-testid={testId}
      data-empty={empty ? 'true' : 'false'}
    >
      <div className="px-5 py-3 border-b border-gray-700/30">
        <h2 className="text-sm font-semibold text-gray-300">{title}</h2>
        <p className="text-[11px] text-gray-500">{description}</p>
      </div>
      {children}
    </div>
  )
}

function formatPct(x: number): string {
  if (!Number.isFinite(x)) return '—'
  return `${(x * 100).toFixed(0)}%`
}

export function RetryBucketPanel({
  summary,
}: {
  summary: OperatorSummaryResponse | null
}) {
  const buckets = loopRetryBuckets(summary)
  const total = loopRetryTotal(summary)
  if (buckets.length === 0) {
    return (
      <SectionCard
        title="Retry bucket state (M6.2)"
        description="Counter octos_loop_retry_total{variant, decision} — every entry is a bounded observation from LoopRetryState."
        testId="retry-bucket-panel"
        empty
      >
        <div className="px-5 py-4 text-[12px] text-gray-500">
          No retry observations reported. The loop has not had to decide on a
          recovery in the current scrape window.
        </div>
      </SectionCard>
    )
  }
  return (
    <SectionCard
      title="Retry bucket state (M6.2)"
      description="Counter octos_loop_retry_total{variant, decision} — every entry is a bounded observation from LoopRetryState. Rows with exhausted_share > 50% are highlighted."
      testId="retry-bucket-panel"
    >
      <div className="px-5 py-2 text-[11px] text-gray-500">
        {total} total retry decisions across {buckets.length} bucket
        {buckets.length === 1 ? '' : 's'}.
      </div>
      <table className="w-full text-sm" data-testid="retry-bucket-table">
        <thead>
          <tr className="text-xs text-gray-500 border-b border-gray-700/30">
            <th className="text-left py-2 px-5 font-medium">variant</th>
            <th className="text-right py-2 px-3 font-medium">total</th>
            <th className="text-right py-2 px-3 font-medium">exhausted</th>
            <th className="text-right py-2 px-3 font-medium">escalate</th>
            <th className="text-right py-2 px-3 font-medium">rotate</th>
            <th className="text-right py-2 px-3 font-medium">compact</th>
            <th className="text-right py-2 px-3 font-medium">continue</th>
            <th className="text-right py-2 px-3 font-medium">grace</th>
            <th className="text-right py-2 px-5 font-medium">exhausted %</th>
          </tr>
        </thead>
        <tbody>
          {buckets.map((bucket) => {
            const hot = bucket.exhausted_share > 0.5
            const warn = !hot && bucket.exhausted_share > 0
            const rowClass = hot
              ? 'bg-red-500/10'
              : warn
                ? 'bg-yellow-500/5'
                : ''
            const pctClass = hot
              ? 'text-red-300'
              : warn
                ? 'text-yellow-300'
                : 'text-gray-400'
            return (
              <tr
                key={bucket.variant}
                data-testid="retry-bucket-row"
                data-variant={bucket.variant}
                data-exhausted-share={bucket.exhausted_share.toFixed(2)}
                data-exhausting={hot ? 'true' : 'false'}
                className={`border-b border-gray-700/15 last:border-0 ${rowClass}`}
              >
                <td className="py-2 px-5 font-mono text-gray-300">
                  {bucket.variant}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-300">
                  {bucket.total}
                </td>
                <td className="py-2 px-3 text-right font-mono text-red-300">
                  {bucket.exhausted}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-400">
                  {bucket.escalate}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-400">
                  {bucket.rotate}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-400">
                  {bucket.compact}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-400">
                  {bucket.continue_count}
                </td>
                <td className="py-2 px-3 text-right font-mono text-gray-400">
                  {bucket.grace}
                </td>
                <td className={`py-2 px-5 text-right font-mono ${pctClass}`}>
                  {formatPct(bucket.exhausted_share)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </SectionCard>
  )
}

export function CompactionPanel({
  summary,
}: {
  summary: OperatorSummaryResponse | null
}) {
  const rows = compactionViolationRows(summary)
  const total = compactionViolationTotal(summary)
  return (
    <SectionCard
      title="Compaction events (M6.3)"
      description="Counter octos_compaction_preservation_violations_total{phase} — non-zero rows are preservation-contract bugs."
      testId="compaction-panel"
      empty={rows.length === 0}
    >
      <div className="px-5 py-3 flex items-baseline gap-6">
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            preservation violations
          </div>
          <div
            data-testid="compaction-violation-total"
            className={`text-2xl font-semibold tabular-nums ${
              total > 0 ? 'text-red-300' : 'text-green-300'
            }`}
          >
            {total}
          </div>
        </div>
      </div>
      {rows.length > 0 ? (
        <table className="w-full text-sm" data-testid="compaction-table">
          <thead>
            <tr className="text-xs text-gray-500 border-b border-gray-700/30">
              <th className="text-left py-2 px-5 font-medium">phase</th>
              <th className="text-right py-2 px-5 font-medium">count</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row, i) => (
              <tr
                key={`${row.phase}|${i}`}
                data-testid="compaction-row"
                data-phase={row.phase}
                className="border-b border-gray-700/15 last:border-0"
              >
                <td className="py-2 px-5 font-mono text-gray-300">
                  {row.phase}
                </td>
                <td className="py-2 px-5 text-right font-mono text-gray-300">
                  {Number(row.count ?? 0)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <div className="px-5 pb-4 text-[12px] text-gray-500">
          No preservation violations observed.
        </div>
      )}
    </SectionCard>
  )
}

export function CredentialPoolPanel({
  summary,
}: {
  summary: OperatorSummaryResponse | null
}) {
  const total = credentialRotationTotal(summary)
  const byReason = credentialRotationsByReason(summary)
  const rows = credentialRotationRows(summary)
  const cooldown = byReason
    .filter((r) =>
      ['rate_limit_cooldown', 'auth_failure'].includes(r.reason),
    )
    .reduce((acc, r) => acc + r.count, 0)
  const active = byReason
    .filter((r) => ['initial_acquire', 'round_robin_advance'].includes(r.reason))
    .reduce((acc, r) => acc + r.count, 0)
  return (
    <SectionCard
      title="Credential pool (M6.5)"
      description="Counter octos_llm_credential_rotation_total{reason, strategy} — rotations per reason. cooldown = rate_limit_cooldown + auth_failure."
      testId="credential-panel"
      empty={total === 0}
    >
      <div className="px-5 py-3 grid grid-cols-3 gap-4">
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            rotations total
          </div>
          <div
            data-testid="credential-total"
            className="text-2xl font-semibold tabular-nums text-gray-200"
          >
            {total}
          </div>
        </div>
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            active selections
          </div>
          <div
            data-testid="credential-active"
            className="text-2xl font-semibold tabular-nums text-green-300"
          >
            {active}
          </div>
        </div>
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            cooldown / failures
          </div>
          <div
            data-testid="credential-cooldown"
            className={`text-2xl font-semibold tabular-nums ${
              cooldown > 0 ? 'text-yellow-300' : 'text-gray-500'
            }`}
          >
            {cooldown}
          </div>
        </div>
      </div>
      {rows.length > 0 ? (
        <table className="w-full text-sm" data-testid="credential-table">
          <thead>
            <tr className="text-xs text-gray-500 border-b border-gray-700/30">
              <th className="text-left py-2 px-5 font-medium">reason</th>
              <th className="text-left py-2 px-3 font-medium">strategy</th>
              <th className="text-right py-2 px-5 font-medium">count</th>
            </tr>
          </thead>
          <tbody>
            {rows.slice(0, 20).map((row, i) => (
              <tr
                key={`${row.reason}|${row.strategy}|${i}`}
                data-testid="credential-row"
                data-reason={row.reason}
                data-strategy={row.strategy}
                className="border-b border-gray-700/15 last:border-0"
              >
                <td className="py-2 px-5 font-mono text-gray-300">
                  {row.reason}
                </td>
                <td className="py-2 px-3 font-mono text-gray-400">
                  {row.strategy}
                </td>
                <td className="py-2 px-5 text-right font-mono text-gray-300">
                  {Number(row.count ?? 0)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <div className="px-5 pb-4 text-[12px] text-gray-500">
          No rotations observed — the pool is either idle or operating on a
          single credential.
        </div>
      )}
    </SectionCard>
  )
}

export function RoutingDecisionPanel({
  summary,
}: {
  summary: OperatorSummaryResponse | null
}) {
  const rows = routingDecisionRows(summary)
  const s = routingDecisionSummary(summary)
  // Very rough saved-budget estimate: every cheap call counts as ~1 strong
  // call avoided. The backend does not expose actual token cost, so this is
  // presented as a relative share, not a dollar figure.
  const savedEstimate = s.cheap
  return (
    <SectionCard
      title="Routing decisions (M6.6)"
      description="Counter octos_routing_decision_total{tier, lane} — cheap vs strong share, per-lane breakdown."
      testId="routing-panel"
      empty={s.total === 0}
    >
      <div className="px-5 py-3 grid grid-cols-4 gap-4">
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            cheap calls
          </div>
          <div
            data-testid="routing-cheap"
            className="text-2xl font-semibold tabular-nums text-green-300"
          >
            {s.cheap}
          </div>
        </div>
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            strong calls
          </div>
          <div
            data-testid="routing-strong"
            className="text-2xl font-semibold tabular-nums text-gray-200"
          >
            {s.strong}
          </div>
        </div>
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            cheap share
          </div>
          <div
            data-testid="routing-cheap-share"
            className="text-2xl font-semibold tabular-nums text-accent"
          >
            {formatPct(s.cheap_share)}
          </div>
        </div>
        <div>
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            strong calls saved
          </div>
          <div
            data-testid="routing-saved"
            className="text-2xl font-semibold tabular-nums text-green-300"
          >
            {savedEstimate}
          </div>
        </div>
      </div>
      {rows.length > 0 ? (
        <table className="w-full text-sm" data-testid="routing-table">
          <thead>
            <tr className="text-xs text-gray-500 border-b border-gray-700/30">
              <th className="text-left py-2 px-5 font-medium">tier</th>
              <th className="text-left py-2 px-3 font-medium">lane</th>
              <th className="text-right py-2 px-5 font-medium">count</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row, i) => (
              <tr
                key={`${row.tier}|${row.lane}|${i}`}
                data-testid="routing-row"
                data-tier={row.tier}
                data-lane={row.lane}
                className="border-b border-gray-700/15 last:border-0"
              >
                <td className="py-2 px-5 font-mono text-gray-300">{row.tier}</td>
                <td className="py-2 px-3 font-mono text-gray-400">{row.lane}</td>
                <td className="py-2 px-5 text-right font-mono text-gray-300">
                  {Number(row.count ?? 0)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <div className="px-5 pb-4 text-[12px] text-gray-500">
          No routing decisions observed — either no chat turns ran in this
          scrape window or the router has not been wired into the active
          provider chain.
        </div>
      )}
    </SectionCard>
  )
}

/**
 * Delegation tree (M6.7) — data contract for delegation is not present on
 * this branch (the M6.7 issue is still open). Render a clearly-labeled stub
 * so operators know the panel is reserved.
 *
 * When M6.7 lands, replace this stub with a real tree renderer that consumes
 * the delegation events backend; the test for the stub text will need to be
 * updated at that time.
 */
export function DelegationTreeStub() {
  return (
    <SectionCard
      title="Delegation tree (M6.7)"
      description="Per-session delegation tree. Wire-up pending M6.7 merge."
      testId="delegation-panel"
      empty
    >
      <div
        data-testid="delegation-stub"
        className="px-5 py-6 text-[13px] text-gray-400 border border-dashed border-gray-700/50 rounded-lg m-4 text-center"
      >
        <div className="font-mono text-gray-300">Pending M6.7 merge</div>
        <div className="mt-1 text-gray-500">
          Delegation tree data is not yet emitted by the backend. Once M6.7
          ships the typed delegation events, this panel will render the tree.
        </div>
      </div>
    </SectionCard>
  )
}
