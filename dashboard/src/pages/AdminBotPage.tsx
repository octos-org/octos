import { useState, useEffect, useCallback } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api'
import { useToast } from '../components/Toast'
import type { MonitorStatus, ProfileResponse } from '../types'

export default function AdminBotPage() {
  const { toast } = useToast()
  const [loading, setLoading] = useState(true)
  const [monitorStatus, setMonitorStatus] = useState<MonitorStatus>({ watchdog_enabled: false, alerts_enabled: false })
  const [profiles, setProfiles] = useState<ProfileResponse[]>([])

  const loadData = useCallback(async () => {
    try {
      const [monitor, profileList] = await Promise.all([
        api.monitorStatus().catch(() => ({ watchdog_enabled: false, alerts_enabled: false })),
        api.listProfiles(),
      ])
      setMonitorStatus(monitor)
      setProfiles(profileList)
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

  const adminProfiles = profiles.filter((profile) => profile.config.admin_mode)
  const activeAdminProfile = adminProfiles.find((profile) => profile.status.running)

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
        <div className="flex items-center justify-between gap-3 mb-4">
          <div>
            <h2 className="text-sm font-semibold text-white">Admin Bot Profile</h2>
            <p className="text-xs text-gray-500 mt-1">
              Active admin bot:{' '}
              <span className={activeAdminProfile ? 'text-green-400' : 'text-gray-400'}>
                {activeAdminProfile ? activeAdminProfile.name : 'None running'}
              </span>
            </p>
          </div>
          <Link
            to="/profiles/new?adminMode=true"
            className="shrink-0 px-3 py-2 text-xs font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
          >
            Create admin profile
          </Link>
        </div>

        {adminProfiles.length > 0 ? (
          <div className="divide-y divide-gray-700/50">
            {adminProfiles.map((profile) => (
              <div key={profile.id} className="flex items-center justify-between gap-3 py-3 first:pt-0 last:pb-0">
                <div className="min-w-0">
                  <Link
                    to={`/profile/${profile.id}`}
                    className="text-sm font-medium text-white hover:text-accent transition"
                  >
                    {profile.name}
                  </Link>
                  <p className="text-xs text-gray-500 font-mono mt-1 truncate">{profile.id}</p>
                </div>
                <span
                  className={`shrink-0 inline-flex px-2 py-0.5 text-[10px] font-medium rounded-full ${
                    profile.status.running
                      ? 'bg-green-500/15 text-green-400'
                      : 'bg-gray-500/15 text-gray-400'
                  }`}
                >
                  {profile.status.running ? 'Running' : 'Stopped'}
                </span>
              </div>
            ))}
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-gray-700/70 px-4 py-5 text-sm text-gray-400">
            No admin-mode profiles found.
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
