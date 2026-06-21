import { useCallback, useEffect, useMemo, useState } from 'react'
import { api } from '../api'
import { useToast } from '../components/Toast'
import type { AdminAuditEntry } from '../types'

const DEFAULT_LIMIT = 50

type AuditFilters = {
  actor: string
  action: string
  from: string
  to: string
}

function formatDateTime(value: string): string {
  const parsed = new Date(value)
  if (Number.isNaN(parsed.getTime())) return value
  return parsed.toLocaleString()
}

function compactJson(value: unknown): string {
  if (value === undefined || value === null) return '-'
  if (typeof value === 'string') return value
  try {
    return JSON.stringify(value)
  } catch {
    return String(value)
  }
}

function actionTone(action: string): string {
  if (action.includes('delete')) return 'bg-red-500/15 text-red-300'
  if (action.includes('toggle')) return 'bg-amber-500/15 text-amber-300'
  if (action.includes('create') || action.includes('add')) return 'bg-green-500/15 text-green-300'
  return 'bg-blue-500/15 text-blue-300'
}

export default function AdminAuditPage() {
  const { toast } = useToast()
  const [entries, setEntries] = useState<AdminAuditEntry[]>([])
  const [total, setTotal] = useState(0)
  const [offset, setOffset] = useState(0)
  const [loading, setLoading] = useState(true)
  const [actor, setActor] = useState('')
  const [action, setAction] = useState('')
  const [from, setFrom] = useState('')
  const [to, setTo] = useState('')

  const pageLabel = useMemo(() => {
    if (total === 0) return '0'
    const start = offset + 1
    const end = Math.min(offset + DEFAULT_LIMIT, total)
    return `${start}-${end} of ${total}`
  }, [offset, total])

  const loadAudit = useCallback(async (
    nextOffset = offset,
    filterOverride?: Partial<AuditFilters>,
  ) => {
    const filters = {
      actor,
      action,
      from,
      to,
      ...filterOverride,
    }
    try {
      setLoading(true)
      const page = await api.listAudit({
        actor: filters.actor,
        action: filters.action,
        from: filters.from,
        to: filters.to,
        limit: DEFAULT_LIMIT,
        offset: nextOffset,
      })
      setEntries(page.entries)
      setTotal(page.total)
      setOffset(page.offset)
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setLoading(false)
    }
  }, [action, actor, from, offset, to, toast])

  useEffect(() => {
    loadAudit(0)
  }, [])

  const applyFilters = (event: React.FormEvent) => {
    event.preventDefault()
    loadAudit(0)
  }

  const clearFilters = () => {
    setActor('')
    setAction('')
    setFrom('')
    setTo('')
    loadAudit(0, { actor: '', action: '', from: '', to: '' })
  }

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h1 className="text-2xl font-bold text-white">Audit</h1>
          <p className="text-sm text-gray-500 mt-1">{pageLabel}</p>
        </div>
        <button
          onClick={() => loadAudit(offset)}
          className="px-3 py-2 text-sm font-medium rounded-lg border border-gray-700 text-gray-300 hover:bg-white/5 transition"
        >
          Refresh
        </button>
      </div>

      <form onSubmit={applyFilters} className="grid grid-cols-1 md:grid-cols-5 gap-3">
        <input
          value={actor}
          onChange={(event) => setActor(event.target.value)}
          aria-label="Actor"
          placeholder="Actor"
          className="input text-sm"
        />
        <input
          value={action}
          onChange={(event) => setAction(event.target.value)}
          aria-label="Action"
          placeholder="Action"
          className="input text-sm"
        />
        <input
          type="date"
          value={from}
          onChange={(event) => setFrom(event.target.value)}
          aria-label="From date"
          className="input text-sm"
        />
        <input
          type="date"
          value={to}
          onChange={(event) => setTo(event.target.value)}
          aria-label="To date"
          className="input text-sm"
        />
        <div className="flex gap-2">
          <button
            type="submit"
            className="flex-1 px-3 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
          >
            Apply
          </button>
          <button
            type="button"
            onClick={clearFilters}
            className="px-3 py-2 text-sm font-medium rounded-lg border border-gray-700 text-gray-400 hover:text-white hover:bg-white/5 transition"
          >
            Clear
          </button>
        </div>
      </form>

      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
        {loading ? (
          <div className="flex items-center justify-center h-64">
            <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
          </div>
        ) : entries.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-gray-500">No audit entries</div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[980px]">
              <thead>
                <tr className="border-b border-gray-700/50">
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Time</th>
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Actor</th>
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Action</th>
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Target</th>
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Before</th>
                  <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">After</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((entry) => (
                  <tr key={entry.id} className="border-b border-gray-700/30 last:border-0 align-top">
                    <td className="px-4 py-3 text-xs text-gray-400 whitespace-nowrap">{formatDateTime(entry.timestamp)}</td>
                    <td className="px-4 py-3 text-sm text-gray-300 font-mono">{entry.actor}</td>
                    <td className="px-4 py-3">
                      <span className={`inline-flex px-2 py-0.5 text-[10px] font-medium rounded-full ${actionTone(entry.action)}`}>
                        {entry.action}
                      </span>
                    </td>
                    <td className="px-4 py-3 text-sm text-white font-mono">{entry.target_id}</td>
                    <td className="px-4 py-3 text-xs text-gray-400 font-mono max-w-xs truncate" title={compactJson(entry.before_summary)}>
                      {compactJson(entry.before_summary)}
                    </td>
                    <td className="px-4 py-3 text-xs text-gray-400 font-mono max-w-xs truncate" title={compactJson(entry.after_summary)}>
                      {compactJson(entry.after_summary)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="flex items-center justify-end gap-2">
        <button
          onClick={() => loadAudit(Math.max(0, offset - DEFAULT_LIMIT))}
          disabled={offset === 0 || loading}
          className="px-3 py-2 text-sm font-medium rounded-lg border border-gray-700 text-gray-300 hover:bg-white/5 transition disabled:opacity-40"
        >
          Previous
        </button>
        <button
          onClick={() => loadAudit(offset + DEFAULT_LIMIT)}
          disabled={offset + DEFAULT_LIMIT >= total || loading}
          className="px-3 py-2 text-sm font-medium rounded-lg border border-gray-700 text-gray-300 hover:bg-white/5 transition disabled:opacity-40"
        >
          Next
        </button>
      </div>
    </div>
  )
}
