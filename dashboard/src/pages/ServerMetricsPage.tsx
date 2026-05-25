import { useState, useEffect, useRef, useMemo } from 'react'
import { api } from '../api'
import type { SystemMetrics } from '../types'

type ProcSortKey = 'pid' | 'name' | 'cpu_percent' | 'memory_bytes'
type SortDir = 'asc' | 'desc'

const POLL_INTERVAL = 5000
const FRESHNESS_TICK = 1000

function formatAge(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000))
  if (seconds < 60) return `${seconds}s ago`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  return `${days}d ago`
}

export function formatFreshnessLabel(updatedAt: number | null, now: number): string {
  if (updatedAt === null) return 'Not updated yet'
  return `Updated ${formatAge(now - updatedAt)}`
}

export function formatLastSuccessfulUpdate(updatedAt: number | null, now: number): string {
  if (updatedAt === null) return 'never'
  return formatAge(now - updatedAt)
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.floor(Math.log(bytes) / Math.log(1024))
  return `${(bytes / Math.pow(1024, i)).toFixed(1)} ${units[i]}`
}

function formatUptime(secs: number): string {
  const days = Math.floor(secs / 86400)
  const hours = Math.floor((secs % 86400) / 3600)
  const mins = Math.floor((secs % 3600) / 60)
  if (days > 0) return `${days}d ${hours}h ${mins}m`
  if (hours > 0) return `${hours}h ${mins}m`
  return `${mins}m`
}

function cpuColor(pct: number): string {
  if (pct >= 80) return 'bg-red-500'
  if (pct >= 50) return 'bg-yellow-500'
  return 'bg-green-500'
}

function diskColor(pct: number): string {
  if (pct >= 90) return 'bg-red-500'
  if (pct >= 70) return 'bg-yellow-500'
  return 'bg-green-500'
}

function SortTh({ label, sortKey, align, current, dir, onSort, last }: {
  label: string
  sortKey: ProcSortKey
  align: 'left' | 'right'
  current: ProcSortKey
  dir: SortDir
  onSort: (k: ProcSortKey) => void
  last?: boolean
}) {
  const active = current === sortKey
  return (
    <th
      className={`${align === 'right' ? 'text-right' : 'text-left'} py-2 ${last ? '' : 'pr-4'} font-medium cursor-pointer select-none hover:text-gray-300 transition-colors ${active ? 'text-accent' : ''}`}
      onClick={() => onSort(sortKey)}
    >
      <span className="inline-flex items-center gap-1">
        {align === 'right' && active && <SortArrow dir={dir} />}
        {label}
        {align === 'left' && active && <SortArrow dir={dir} />}
      </span>
    </th>
  )
}

function SortArrow({ dir }: { dir: SortDir }) {
  return (
    <svg className="w-3 h-3 inline" viewBox="0 0 12 12" fill="currentColor">
      {dir === 'desc'
        ? <path d="M6 8L2 4h8z" />
        : <path d="M6 4l4 4H2z" />}
    </svg>
  )
}

function ProgressBar({ percent, colorFn }: { percent: number; colorFn: (pct: number) => string }) {
  return (
    <div className="w-full h-2.5 bg-gray-700 rounded-full overflow-hidden">
      <div
        className={`h-full rounded-full transition-all duration-500 ease-in-out ${colorFn(percent)}`}
        style={{ width: `${Math.min(percent, 100)}%` }}
      />
    </div>
  )
}

export default function ServerMetricsPage() {
  const [metrics, setMetrics] = useState<SystemMetrics | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(true)
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null)
  const [now, setNow] = useState(() => Date.now())
  const [procSort, setProcSort] = useState<ProcSortKey>('memory_bytes')
  const [procDir, setProcDir] = useState<SortDir>('desc')
  const [procOpen, setProcOpen] = useState(false)
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const procOpenRef = useRef(false)

  const fetchMetrics = async () => {
    try {
      const data = await api.systemMetrics({ procs: procOpenRef.current })
      const fetchedAt = Date.now()
      setMetrics(data)
      setLastUpdatedAt(fetchedAt)
      setNow(fetchedAt)
      setError(null)
    } catch (e: any) {
      setError(e.message)
    }
  }

  useEffect(() => {
    fetchMetrics()
    timerRef.current = setInterval(fetchMetrics, POLL_INTERVAL)
    const freshnessTimer = setInterval(() => setNow(Date.now()), FRESHNESS_TICK)
    return () => {
      if (timerRef.current) clearInterval(timerRef.current)
      clearInterval(freshnessTimer)
    }
  }, [])

  const toggleLive = () => {
    if (live) {
      if (timerRef.current) clearInterval(timerRef.current)
      timerRef.current = null
    } else {
      fetchMetrics()
      timerRef.current = setInterval(fetchMetrics, POLL_INTERVAL)
    }
    setLive(!live)
  }

  const toggleProcSort = (key: ProcSortKey) => {
    if (procSort === key) {
      setProcDir(d => d === 'desc' ? 'asc' : 'desc')
    } else {
      setProcSort(key)
      setProcDir('desc')
    }
  }

  const sortedProcs = useMemo(() => {
    if (!metrics) return []
    return [...metrics.top_processes].sort((a, b) => {
      const av = a[procSort]
      const bv = b[procSort]
      const cmp = typeof av === 'string'
        ? (av as string).localeCompare(bv as string)
        : (av as number) - (bv as number)
      return procDir === 'desc' ? -cmp : cmp
    })
  }, [metrics, procSort, procDir])

  if (!metrics && !error) {
    return (
      <div className="flex items-center justify-center h-64 text-gray-500">
        Loading system metrics...
      </div>
    )
  }

  if (!metrics && error) {
    return (
      <div className="flex flex-col items-center justify-center h-64 text-center">
        <p className="text-red-400 mb-2">Failed to load metrics</p>
        <p className="text-sm text-gray-500">{error}</p>
      </div>
    )
  }

  const m = metrics!
  const memPercent = m.memory.total_bytes > 0
    ? (m.memory.used_bytes / m.memory.total_bytes) * 100
    : 0
  const swapPercent = m.swap.total_bytes > 0
    ? (m.swap.used_bytes / m.swap.total_bytes) * 100
    : 0

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-white">{m.platform.hostname || 'Server'}</h1>
          <p className="text-sm text-gray-500">
            {m.platform.os} {m.platform.os_version} &middot; up {formatUptime(m.platform.uptime_secs)}
          </p>
        </div>
        <div className="flex items-center gap-3">
          <span className="text-xs text-gray-500">
            {formatFreshnessLabel(lastUpdatedAt, now)}
          </span>
          <button
            onClick={toggleLive}
            className="flex items-center gap-2 px-3 py-1.5 text-xs font-medium rounded-lg border border-gray-700 hover:bg-white/5 transition"
          >
            <span className={`inline-block w-2 h-2 rounded-full ${live ? 'bg-green-500 animate-pulse' : 'bg-gray-600'}`} />
            {live ? 'Live' : 'Paused'}
          </button>
        </div>
      </div>

      {error && (
        <div className="text-xs text-yellow-500 bg-yellow-500/10 rounded-lg px-3 py-2">
          Update failed: {error}. Last successful update: {formatLastSuccessfulUpdate(lastUpdatedAt, now)}.
        </div>
      )}

      {/* CPU + Memory grid */}
      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        {/* CPU Card */}
        <div className="bg-surface border border-gray-700/50 rounded-xl p-5 space-y-4">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-semibold text-gray-300">CPU</h2>
            <span className="text-xs text-gray-500">{m.cpu.core_count} cores</span>
          </div>
          <div className="space-y-2">
            <div className="flex items-center justify-between text-sm">
              <span className="text-gray-400">Usage</span>
              <span className="text-white font-mono">{m.cpu.usage_percent.toFixed(1)}%</span>
            </div>
            <ProgressBar percent={m.cpu.usage_percent} colorFn={cpuColor} />
          </div>
          <p className="text-xs text-gray-600 truncate" title={m.cpu.brand}>{m.cpu.brand}</p>
        </div>

        {/* Memory Card */}
        <div className="bg-surface border border-gray-700/50 rounded-xl p-5 space-y-4">
          <h2 className="text-sm font-semibold text-gray-300">Memory</h2>
          <div className="space-y-2">
            <div className="flex items-center justify-between text-sm">
              <span className="text-gray-400">RAM</span>
              <span className="text-white font-mono">
                {formatBytes(m.memory.used_bytes)} / {formatBytes(m.memory.total_bytes)}
              </span>
            </div>
            <ProgressBar percent={memPercent} colorFn={cpuColor} />
            <p className="text-xs text-gray-500">
              {formatBytes(m.memory.available_bytes)} available
            </p>
          </div>

          {m.swap.total_bytes > 0 && (
            <div className="space-y-2 pt-2 border-t border-gray-700/30">
              <div className="flex items-center justify-between text-sm">
                <span className="text-gray-400">Swap</span>
                <span className="text-white font-mono">
                  {formatBytes(m.swap.used_bytes)} / {formatBytes(m.swap.total_bytes)}
                </span>
              </div>
              <ProgressBar percent={swapPercent} colorFn={cpuColor} />
            </div>
          )}
        </div>
      </div>

      {/* Top Processes */}
      <div className="bg-surface border border-gray-700/50 rounded-xl">
        <button
          onClick={() => {
            const next = !procOpen
            procOpenRef.current = next
            setProcOpen(next)
            if (next) fetchMetrics()
          }}
          className="w-full flex items-center justify-between p-5 text-left"
        >
          <h2 className="text-sm font-semibold text-gray-300">Top Processes</h2>
          <svg
            className={`w-4 h-4 text-gray-500 transition-transform duration-200 ${procOpen ? 'rotate-180' : ''}`}
            fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}
          >
            <path strokeLinecap="round" strokeLinejoin="round" d="M19 9l-7 7-7-7" />
          </svg>
        </button>
        {procOpen && (
          <div className="px-5 pb-5 overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="text-xs text-gray-500 border-b border-gray-700/30">
                  <SortTh label="PID" sortKey="pid" align="left" current={procSort} dir={procDir} onSort={toggleProcSort} />
                  <SortTh label="Name" sortKey="name" align="left" current={procSort} dir={procDir} onSort={toggleProcSort} />
                  <SortTh label="CPU" sortKey="cpu_percent" align="right" current={procSort} dir={procDir} onSort={toggleProcSort} />
                  <SortTh label="Memory" sortKey="memory_bytes" align="right" current={procSort} dir={procDir} onSort={toggleProcSort} last />
                </tr>
              </thead>
              <tbody>
                {sortedProcs.map((proc, i) => (
                  <tr key={proc.pid} className={i % 2 === 0 ? 'bg-white/[0.02]' : ''}>
                    <td className="py-1.5 pr-4 font-mono text-gray-500">{proc.pid}</td>
                    <td className="py-1.5 pr-4 text-gray-300 truncate max-w-[200px]" title={proc.name}>
                      {proc.name}
                    </td>
                    <td className="py-1.5 pr-4 text-right font-mono">
                      <span className={proc.cpu_percent >= 80 ? 'text-red-400' : proc.cpu_percent >= 50 ? 'text-yellow-400' : 'text-white'}>
                        {proc.cpu_percent.toFixed(1)}%
                      </span>
                    </td>
                    <td className="py-1.5 text-right font-mono text-white">
                      {formatBytes(proc.memory_bytes)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Disks Card */}
      {m.disks.length > 0 && (
        <div className="bg-surface border border-gray-700/50 rounded-xl p-5 space-y-4">
          <h2 className="text-sm font-semibold text-gray-300">Disks</h2>
          <div className="space-y-4">
            {m.disks.map((disk, i) => {
              const usedPct = disk.total_bytes > 0
                ? (disk.used_bytes / disk.total_bytes) * 100
                : 0
              return (
                <div key={i} className="space-y-2">
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-400 truncate mr-3" title={disk.mount_point}>
                      {disk.mount_point}
                      <span className="text-gray-600 ml-1.5 text-xs">({disk.file_system})</span>
                    </span>
                    <span className="text-white font-mono whitespace-nowrap">
                      {formatBytes(disk.used_bytes)} / {formatBytes(disk.total_bytes)}
                    </span>
                  </div>
                  <ProgressBar percent={usedPct} colorFn={diskColor} />
                </div>
              )
            })}
          </div>
        </div>
      )}
    </div>
  )
}
