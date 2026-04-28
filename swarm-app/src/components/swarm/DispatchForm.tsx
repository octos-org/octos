import { useState } from 'react'
import { swarmApi, type DispatchRequest, type DispatchResponse } from '../../api/swarm'
import { parseEditorBody } from './ContractEditor'

interface Props {
  /** Current contract body from the Author tab. */
  contractBody: string
  /** Callback fired on successful dispatch — parent surfaces the id. */
  onDispatched?: (resp: DispatchResponse) => void
}

/**
 * POST form that surfaces the parsed contract + editable pool / budget
 * overrides and submits the dispatch to `/api/swarm/dispatch`. Returns
 * the `dispatch_id` to the parent on success.
 */
export default function DispatchForm({ contractBody, onDispatched }: Props) {
  const [status, setStatus] = useState<'idle' | 'submitting' | 'ok' | 'error'>(
    'idle',
  )
  const [message, setMessage] = useState<string | null>(null)
  const [result, setResult] = useState<DispatchResponse | null>(null)
  const [overrideDispatchId, setOverrideDispatchId] = useState('')
  const [overrideContractId, setOverrideContractId] = useState('')
  const [overrideBudget, setOverrideBudget] = useState('')

  const handleSubmit = async (ev: React.FormEvent<HTMLFormElement>) => {
    ev.preventDefault()
    setStatus('submitting')
    setMessage(null)
    setResult(null)
    const { parsed, error, issues } = parseEditorBody(contractBody)
    if (error || !parsed) {
      setStatus('error')
      setMessage(`Invalid contract body: ${error ?? 'parse failed'}`)
      return
    }
    if (issues.length > 0) {
      setStatus('error')
      setMessage(`Contract has ${issues.length} validation issue(s). Fix in the Author tab.`)
      return
    }
    const req: DispatchRequest = {
      ...parsed,
      dispatch_id: overrideDispatchId.trim() || parsed.dispatch_id,
      contract_id: overrideContractId.trim() || parsed.contract_id,
    }
    if (overrideBudget.trim()) {
      const n = Number(overrideBudget)
      if (Number.isFinite(n) && n > 0) {
        req.budget = { ...(req.budget ?? {}), per_dispatch_usd: n }
      }
    }
    try {
      const resp = await swarmApi.dispatch(req)
      setStatus('ok')
      setResult(resp)
      setMessage(`Dispatched ${resp.dispatch_id}: ${resp.completed_subtasks}/${resp.total_subtasks} completed`)
      if (onDispatched) onDispatched(resp)
    } catch (e: unknown) {
      setStatus('error')
      setMessage(e instanceof Error ? e.message : String(e))
    }
  }

  return (
    <form
      onSubmit={handleSubmit}
      data-testid="swarm-dispatch-form"
      className="flex flex-col gap-4"
    >
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
        <Field
          label="Override dispatch_id (optional)"
          value={overrideDispatchId}
          onChange={setOverrideDispatchId}
          placeholder="defaults to value from contract"
          testId="swarm-override-dispatch"
        />
        <Field
          label="Override contract_id (optional)"
          value={overrideContractId}
          onChange={setOverrideContractId}
          placeholder="defaults to value from contract"
          testId="swarm-override-contract"
        />
        <Field
          label="Per-dispatch USD ceiling (optional)"
          value={overrideBudget}
          onChange={setOverrideBudget}
          placeholder="e.g. 0.50"
          testId="swarm-override-budget"
        />
      </div>
      <button
        type="submit"
        disabled={status === 'submitting'}
        data-testid="swarm-dispatch-submit"
        className="self-start rounded-lg bg-accent/15 px-4 py-2 text-sm font-semibold text-accent hover:bg-accent/25 disabled:cursor-not-allowed disabled:opacity-60"
      >
        {status === 'submitting' ? 'Dispatching…' : 'Dispatch swarm'}
      </button>
      {status === 'error' && message && (
        <div
          data-testid="swarm-dispatch-error"
          className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300"
        >
          {message}
        </div>
      )}
      {status === 'ok' && result && (
        <div
          data-testid="swarm-dispatch-result"
          className="rounded-lg border border-green-500/40 bg-green-500/10 px-3 py-2 text-xs text-green-300"
        >
          <div className="font-semibold">dispatch_id: {result.dispatch_id}</div>
          <div>{message}</div>
        </div>
      )}
    </form>
  )
}

function Field({
  label,
  value,
  onChange,
  placeholder,
  testId,
}: {
  label: string
  value: string
  onChange: (v: string) => void
  placeholder?: string
  testId?: string
}) {
  return (
    <label className="flex flex-col gap-1 text-xs text-gray-400">
      <span>{label}</span>
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        data-testid={testId}
        className="rounded-lg border border-gray-700/60 bg-surface px-3 py-1.5 text-sm text-gray-200 focus:border-accent/60 focus:outline-none"
      />
    </label>
  )
}
