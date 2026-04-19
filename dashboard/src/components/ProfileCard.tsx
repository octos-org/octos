import { useState } from 'react'
import { Link } from 'react-router-dom'
import type { ProfileResponse } from '../types'
import { CHANNEL_COLORS, CHANNEL_LABELS } from '../types'
import StatusBadge from './StatusBadge'

interface Props {
  profile: ProfileResponse
  subAccounts?: ProfileResponse[]
  onStart: (id: string) => void
  onStop: (id: string) => void
}

function formatUptime(secs: number | null): string {
  if (!secs) return ''
  const days = Math.floor(secs / 86400)
  const hours = Math.floor((secs % 86400) / 3600)
  const mins = Math.floor((secs % 3600) / 60)
  if (days > 0) return `${days}d ${hours}h`
  if (hours > 0) return `${hours}h ${mins}m`
  return `${mins}m`
}

export default function ProfileCard({ profile, subAccounts = [], onStart, onStop }: Props) {
  const [expanded, setExpanded] = useState(true)
  const channels = profile.config.channels || []
  const provider = profile.config.llm?.primary?.family_id || 'anthropic'
  const model = profile.config.llm?.primary?.model_id || 'default'

  return (
    <div className="bg-surface rounded-xl border border-gray-700/50 p-5 hover:border-gray-600/50 transition-colors group flex flex-col">
      <div className="flex items-start justify-between mb-3">
        <Link
          to={`/profile/${profile.id}`}
          className="text-white font-semibold hover:text-accent transition-colors"
        >
          {profile.name}
        </Link>
        <StatusBadge running={profile.status.running} />
      </div>

      <div className="space-y-2 mb-4">
        <div className="flex items-center gap-2 text-xs text-gray-400">
          <span className="text-gray-500">Provider:</span>
          <span className="capitalize">{provider}</span>
        </div>
        <div className="flex items-center gap-2 text-xs text-gray-400">
          <span className="text-gray-500">Model:</span>
          <span className="truncate max-w-[140px]">{model}</span>
        </div>
        {profile.status.running && profile.status.pid && (
          <div className="flex items-center gap-2 text-xs text-gray-400">
            <span className="text-gray-500">PID:</span>
            <span className="font-mono">{profile.status.pid}</span>
            {profile.status.uptime_secs ? (
              <span className="text-gray-600 ml-1">({formatUptime(profile.status.uptime_secs)})</span>
            ) : null}
          </div>
        )}
      </div>

      {channels.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mb-4">
          {channels.map((ch, i) => {
            const type = ch.type as keyof typeof CHANNEL_COLORS
            return (
              <span
                key={i}
                className={`${CHANNEL_COLORS[type] || 'bg-gray-500'} text-white text-[10px] font-bold px-1.5 py-0.5 rounded`}
              >
                {CHANNEL_LABELS[type] || ch.type.toUpperCase().slice(0, 2)}
              </span>
            )
          })}
        </div>
      )}

      <div className="flex gap-2 mt-auto pt-4">
        {profile.status.running ? (
          <button
            onClick={() => onStop(profile.id)}
            className="flex-1 px-3 py-1.5 text-xs font-medium rounded-lg bg-red-500/10 text-red-400 hover:bg-red-500/20 border border-red-500/20 transition"
          >
            Stop
          </button>
        ) : (
          <button
            onClick={() => onStart(profile.id)}
            className="flex-1 px-3 py-1.5 text-xs font-medium rounded-lg bg-green-500/10 text-green-400 hover:bg-green-500/20 border border-green-500/20 transition"
          >
            Start
          </button>
        )}
        <Link
          to={`/profile/${profile.id}`}
          className="px-3 py-1.5 text-xs font-medium rounded-lg bg-white/5 text-gray-400 hover:bg-white/10 hover:text-white border border-gray-700/50 transition"
        >
          Configure
        </Link>
      </div>

      {/* Sub-accounts section */}
      {subAccounts.length > 0 && (
        <div className="mt-4 pt-3 border-t border-gray-700/30">
          <button
            onClick={() => setExpanded(!expanded)}
            className="flex items-center gap-2 text-xs text-gray-400 hover:text-gray-300 transition-colors w-full"
          >
            <svg
              className={`w-3 h-3 transition-transform ${expanded ? 'rotate-90' : ''}`}
              fill="none"
              viewBox="0 0 24 24"
              stroke="currentColor"
              strokeWidth={2}
            >
              <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
            </svg>
            <span className="font-medium">
              {subAccounts.length} sub-account{subAccounts.length !== 1 ? 's' : ''}
            </span>
            <span className="ml-auto text-gray-600">
              {subAccounts.filter(s => s.status.running).length} running
            </span>
          </button>

          {expanded && (
            <div className="mt-2 space-y-2">
              {subAccounts.map((sub) => (
                <SubAccountRow
                  key={sub.id}
                  sub={sub}
                  onStart={onStart}
                  onStop={onStop}
                />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function SubAccountRow({
  sub,
  onStart,
  onStop,
}: {
  sub: ProfileResponse
  onStart: (id: string) => void
  onStop: (id: string) => void
}) {
  const channels = sub.config.channels || []
  const shortName = sub.name

  return (
    <div className="flex items-center gap-2 py-1.5 px-2 rounded-lg bg-white/[0.02] hover:bg-white/[0.04] transition-colors">
      <StatusBadge running={sub.status.running} className="shrink-0" />

      <Link
        to={`/profile/${sub.id}`}
        className="text-xs text-gray-300 hover:text-accent transition-colors truncate min-w-0 flex-1"
      >
        {shortName}
      </Link>

      {channels.length > 0 && (
        <div className="flex gap-1 shrink-0">
          {channels.map((ch, i) => {
            const type = ch.type as keyof typeof CHANNEL_COLORS
            return (
              <span
                key={i}
                className={`${CHANNEL_COLORS[type] || 'bg-gray-500'} text-white text-[9px] font-bold px-1 py-0 rounded`}
              >
                {CHANNEL_LABELS[type] || ch.type.toUpperCase().slice(0, 2)}
              </span>
            )
          })}
        </div>
      )}

      <button
        onClick={() => sub.status.running ? onStop(sub.id) : onStart(sub.id)}
        className={`shrink-0 px-2 py-0.5 text-[10px] font-medium rounded ${
          sub.status.running
            ? 'bg-red-500/10 text-red-400 hover:bg-red-500/20'
            : 'bg-green-500/10 text-green-400 hover:bg-green-500/20'
        } transition`}
      >
        {sub.status.running ? 'Stop' : 'Start'}
      </button>
    </div>
  )
}
