import { useState, useEffect, useCallback } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api'
import { useToast } from '../components/Toast'
import type { MonitorProfileStatus, MonitorStatus, ProfileResponse } from '../types'

type MonitorOverride = 'inherit' | 'enabled' | 'disabled'

const EMPTY_MONITOR_STATUS: MonitorStatus = {
  watchdog_enabled: false,
  alerts_enabled: false,
  profiles: [],
}

export default function AdminBotPage() {
  const { toast } = useToast()
  const [loading, setLoading] = useState(true)
  const [savingProfileId, setSavingProfileId] = useState<string | null>(null)
  const [monitorStatus, setMonitorStatus] = useState<MonitorStatus>(EMPTY_MONITOR_STATUS)
  const [profiles, setProfiles] = useState<ProfileResponse[]>([])

  const loadData = useCallback(async () => {
    try {
      const [monitor, profileList] = await Promise.all([
        api.monitorStatus().catch(() => EMPTY_MONITOR_STATUS),
        api.listProfiles(),
      ])
      setMonitorStatus({ ...EMPTY_MONITOR_STATUS, ...monitor, profiles: monitor.profiles ?? [] })
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

  const handleProfileMonitorChange = async (
    profile: MonitorProfileStatus,
    field: 'watchdog' | 'alerts',
    value: MonitorOverride,
  ) => {
    setSavingProfileId(profile.id)
    try {
      const updated = await api.updateProfileMonitor(profile.id, { [field]: value })
      setMonitorStatus((prev) => ({
        ...prev,
        profiles: prev.profiles.map((item) => (item.id === updated.id ? updated : item)),
      }))
      toast(`${profile.name} ${field} set to ${labelOverride(value).toLowerCase()}`)
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setSavingProfileId(null)
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
    <div className="max-w-5xl">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-white">Monitor & Watchdog</h1>
        <p className="text-sm text-gray-500 mt-1">
          Set the system default, then override watchdog and alerts for individual profiles.
        </p>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-5 mb-5">
        <h2 className="text-sm font-semibold text-white mb-4">System Default</h2>
        <div className="space-y-4">
          <Toggle
            label="Watchdog enabled"
            description="Fallback restart behavior for profiles without an override"
            checked={monitorStatus.watchdog_enabled}
            onChange={handleToggleWatchdog}
          />
          <Toggle
            label="Alerts enabled"
            description="Fallback alert behavior for profiles without an override"
            checked={monitorStatus.alerts_enabled}
            onChange={handleToggleAlerts}
          />
        </div>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-5 mb-5">
        <h2 className="text-sm font-semibold text-white mb-4">Profile Overrides</h2>
        {monitorStatus.profiles.length === 0 ? (
          <p className="text-sm text-gray-500">No profiles found.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead className="text-xs uppercase text-gray-500">
                <tr className="border-b border-gray-700/50">
                  <th className="pb-2 font-medium">Profile</th>
                  <th className="pb-2 font-medium">Status</th>
                  <th className="pb-2 font-medium">Watchdog</th>
                  <th className="pb-2 font-medium">Alerts</th>
                </tr>
              </thead>
              <tbody>
                {monitorStatus.profiles.map((profile) => (
                  <tr key={profile.id} className="border-b border-gray-800/80 last:border-0">
                    <td className="py-3 pr-4">
                      <div className="font-medium text-white">{profile.name}</div>
                      <div className="font-mono text-xs text-gray-500">{profile.id}</div>
                    </td>
                    <td className="py-3 pr-4">
                      <span className={profile.enabled ? 'text-emerald-300' : 'text-gray-500'}>
                        {profile.enabled ? 'Enabled' : 'Disabled'}
                      </span>
                    </td>
                    <td className="py-3 pr-4">
                      <MonitorOverrideSelect
                        value={overrideValue(profile.watchdog_override)}
                        effective={profile.watchdog_enabled}
                        disabled={savingProfileId === profile.id}
                        onChange={(value) => handleProfileMonitorChange(profile, 'watchdog', value)}
                      />
                    </td>
                    <td className="py-3">
                      <MonitorOverrideSelect
                        value={overrideValue(profile.alerts_override)}
                        effective={profile.alerts_enabled}
                        disabled={savingProfileId === profile.id}
                        onChange={(value) => handleProfileMonitorChange(profile, 'alerts', value)}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
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

function overrideValue(value: boolean | null | undefined): MonitorOverride {
  if (value == null) return 'inherit'
  return value ? 'enabled' : 'disabled'
}

function labelOverride(value: MonitorOverride) {
  switch (value) {
    case 'enabled':
      return 'Enabled'
    case 'disabled':
      return 'Disabled'
    case 'inherit':
      return 'Inherit'
  }
}

function MonitorOverrideSelect({
  value,
  effective,
  disabled,
  onChange,
}: {
  value: MonitorOverride
  effective: boolean
  disabled: boolean
  onChange: (value: MonitorOverride) => void
}) {
  return (
    <div className="flex flex-col gap-1">
      <select
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value as MonitorOverride)}
        className="w-36 rounded-lg border border-gray-700 bg-background px-2 py-1.5 text-sm text-white focus:border-accent focus:outline-none disabled:opacity-50"
      >
        <option value="inherit">Inherit</option>
        <option value="enabled">Enabled</option>
        <option value="disabled">Disabled</option>
      </select>
      <span className="text-xs text-gray-500">
        Effective: {effective ? 'enabled' : 'disabled'}
      </span>
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
