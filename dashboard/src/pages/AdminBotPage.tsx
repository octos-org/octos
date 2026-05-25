import { useState, useEffect, useCallback } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api'
import StatusBadge from '../components/StatusBadge'
import { useToast } from '../components/Toast'
import type { MonitorStatus, ProfileResponse } from '../types'

function isAdminProfile(profile: ProfileResponse) {
  return profile.config.admin_mode === true
}

export default function AdminBotPage() {
  const { toast } = useToast()
  const [loading, setLoading] = useState(true)
  const [monitorStatus, setMonitorStatus] = useState<MonitorStatus>({ watchdog_enabled: false, alerts_enabled: false })
  const [adminProfiles, setAdminProfiles] = useState<ProfileResponse[]>([])

  const loadData = useCallback(async () => {
    try {
      const [monitor, profiles] = await Promise.all([
        api.monitorStatus().catch(() => ({ watchdog_enabled: false, alerts_enabled: false })),
        api.listProfiles(),
      ])
      setMonitorStatus(monitor)
      setAdminProfiles(profiles.filter(isAdminProfile))
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setLoading(false)
    }
  }, [toast])

  useEffect(() => {
    loadData()
  }, [loadData])

  const handleToggleWatchdog = async (enabled: boolean) => {
    try {
      const result = await api.toggleWatchdog(enabled)
      setMonitorStatus((prev) => ({ ...prev, watchdog_enabled: result.watchdog_enabled }))
      toast(`Watchdog ${result.watchdog_enabled ? 'enabled' : 'disabled'}`)
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }

  const handleToggleAlerts = async (enabled: boolean) => {
    try {
      const result = await api.toggleAlerts(enabled)
      setMonitorStatus((prev) => ({ ...prev, alerts_enabled: result.alerts_enabled }))
      toast(`Alerts ${result.alerts_enabled ? 'enabled' : 'disabled'}`)
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  const activeProfiles = adminProfiles.filter((profile) => profile.status.running)

  return (
    <div className="max-w-3xl">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-white">Monitor & Watchdog</h1>
        <p className="text-sm text-gray-500 mt-1">
          Controls for the system-wide watchdog and alerting. These apply to all gateway profiles.
        </p>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-5 mb-5">
        <h2 className="text-sm font-semibold text-white mb-4">Watchdog & Alerts</h2>
        <div className="space-y-4">
          <Toggle
            label="Watchdog enabled"
            description="Automatically restart crashed gateways"
            checked={monitorStatus.watchdog_enabled}
            onChange={handleToggleWatchdog}
          />
          <Toggle
            label="Alerts enabled"
            description="Send proactive alerts when gateways crash or become unhealthy"
            checked={monitorStatus.alerts_enabled}
            onChange={handleToggleAlerts}
          />
        </div>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-5">
        <div className="flex items-start justify-between gap-3 mb-4">
          <div>
            <h2 className="text-sm font-semibold text-white">Admin Bot Profiles</h2>
            <p className="text-xs text-gray-500 mt-1">
              Active admin: <span className="text-gray-300">
                {activeProfiles.length > 0
                  ? activeProfiles.map((profile) => profile.name).join(', ')
                  : 'None running'}
              </span>
            </p>
          </div>
          <Link
            to="/profiles/new?adminMode=true"
            className="shrink-0 px-3 py-1.5 text-xs font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
          >
            Create admin profile
          </Link>
        </div>

        {adminProfiles.length > 0 ? (
          <div className="divide-y divide-gray-700/50">
            {adminProfiles.map((profile) => (
              <div key={profile.id} className="py-3 first:pt-0 last:pb-0 flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <Link
                    to={`/profile/${profile.id}`}
                    className="text-sm font-medium text-white hover:text-accent transition-colors truncate block"
                  >
                    {profile.name}
                  </Link>
                  <p className="text-xs text-gray-500 font-mono truncate">{profile.id}</p>
                </div>
                <div className="flex items-center gap-3 shrink-0">
                  <StatusBadge running={profile.status.running} />
                  <Link
                    to={`/profile/${profile.id}`}
                    className="px-3 py-1.5 text-xs font-medium rounded-lg bg-white/5 text-gray-400 hover:bg-white/10 hover:text-white border border-gray-700/50 transition"
                  >
                    Open
                  </Link>
                </div>
              </div>
            ))}
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-gray-700/70 p-4 text-sm text-gray-400">
            No admin bot profiles yet.
          </div>
        )}
      </div>
    </div>
  )
}

function Toggle({
  label,
  description,
  checked,
  onChange,
}: {
  label: string
  description: string
  checked: boolean
  onChange: (v: boolean) => void
}) {
  return (
    <div className="flex items-center justify-between">
      <div>
        <p className="text-sm text-white">{label}</p>
        <p className="text-xs text-gray-500">{description}</p>
      </div>
      <button
        type="button"
        onClick={() => onChange(!checked)}
        className={`relative inline-flex h-5 w-9 items-center rounded-full transition-colors ${
          checked ? 'bg-accent' : 'bg-gray-600'
        }`}
      >
        <span
          className={`inline-block h-3.5 w-3.5 transform rounded-full bg-white transition-transform ${
            checked ? 'translate-x-4' : 'translate-x-0.5'
          }`}
        />
      </button>
    </div>
  )
}
