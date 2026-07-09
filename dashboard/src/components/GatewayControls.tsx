import type { ProcessStatus } from '../types'
import StatusBadge from './StatusBadge'

interface Props {
  status: ProcessStatus
  loading: boolean
  onStart: () => void
  onStop: () => void
  onRestart: () => void
}

function formatUptime(secs: number | null): string {
  if (!secs) return '0m'
  const days = Math.floor(secs / 86400)
  const hours = Math.floor((secs % 86400) / 3600)
  const mins = Math.floor((secs % 3600) / 60)
  if (days > 0) return `${days}d ${hours}h ${mins}m`
  if (hours > 0) return `${hours}h ${mins}m`
  return `${mins}m`
}

export default function GatewayControls({ status, loading, onStart, onStop, onRestart }: Props) {
  return (
    <div className="bg-surface rounded-xl border border-gray-700/50 p-5">
      <div className="flex items-center justify-between mb-4">
        <h3 className="text-sm font-semibold text-white">Gateway Process</h3>
        <StatusBadge running={status.running} status={status.status} error={status.error} />
      </div>

      {status.status === 'configuration_error' && status.error && (
        <p className="mb-4 text-xs text-amber-300">{status.error}</p>
      )}

      {status.running && (
        <div className="grid grid-cols-2 gap-3 mb-4 text-xs">
          <div>
            <span className="text-gray-500 block">PID</span>
            <span className="text-gray-300 font-mono">{status.pid}</span>
          </div>
          <div>
            <span className="text-gray-500 block">Uptime</span>
            <span className="text-gray-300">{formatUptime(status.uptime_secs)}</span>
          </div>
        </div>
      )}

      <div className="flex gap-2">
        {status.running ? (
          <>
            <button
              onClick={onStop}
              disabled={loading}
              className="flex-1 px-3 py-2 text-xs font-medium rounded-lg bg-red-500/10 text-red-400 hover:bg-red-500/20 border border-red-500/20 transition disabled:opacity-50"
            >
              Stop
            </button>
            <button
              onClick={onRestart}
              disabled={loading}
              className="flex-1 px-3 py-2 text-xs font-medium rounded-lg bg-yellow-500/10 text-yellow-400 hover:bg-yellow-500/20 border border-yellow-500/20 transition disabled:opacity-50"
            >
              Restart
            </button>
          </>
        ) : (
          <button
            onClick={onStart}
            disabled={loading}
            className="flex-1 px-3 py-2 text-xs font-medium rounded-lg bg-green-500/10 text-green-400 hover:bg-green-500/20 border border-green-500/20 transition disabled:opacity-50"
          >
            Start
          </button>
        )}
      </div>
    </div>
  )
}
