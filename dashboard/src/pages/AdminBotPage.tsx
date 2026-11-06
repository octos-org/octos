import { useState, useEffect, useCallback, useRef } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api'
import StatusBadge from '../components/StatusBadge'
import { useToast } from '../components/Toast'
import type { MonitorProfileStatus, MonitorStatus, ProfileResponse } from '../types'

type MonitorOverride = 'inherit' | 'enabled' | 'disabled'
type MonitorField = 'watchdog' | 'alerts'

const EMPTY_MONITOR_STATUS: MonitorStatus = {
  watchdog_enabled: false,
  alerts_enabled: false,
  profiles: [],
}

function normalizeStatus(monitor: Partial<MonitorStatus>): MonitorStatus {
  return { ...EMPTY_MONITOR_STATUS, ...monitor, profiles: monitor.profiles ?? [] }
}

/**
 * A profile without an override follows the system default — the server's
 * own rule (`override.unwrap_or(system)` in admin.rs). Applied when the
 * default changes, so inherited rows do not keep showing the old default.
 */
export function inheritDefault(
  profile: MonitorProfileStatus,
  status: Pick<MonitorStatus, 'watchdog_enabled' | 'alerts_enabled'>,
): MonitorProfileStatus {
  return {
    ...profile,
    watchdog_enabled: profile.watchdog_override ?? status.watchdog_enabled,
    alerts_enabled: profile.alerts_override ?? status.alerts_enabled,
  }
}

export function withSystemDefault(
  status: MonitorStatus,
  field: MonitorField,
  enabled: boolean,
): MonitorStatus {
  const next = {
    ...status,
    ...(field === 'watchdog' ? { watchdog_enabled: enabled } : { alerts_enabled: enabled }),
  }
  return { ...next, profiles: next.profiles.map((profile) => inheritDefault(profile, next)) }
}

export default function AdminBotPage() {
  const { toast } = useToast()
  const [loading, setLoading] = useState(true)
  const [savingProfileId, setSavingProfileId] = useState<string | null>(null)
  const [monitorStatus, setMonitorStatus] = useState<MonitorStatus>(EMPTY_MONITOR_STATUS)
  const [profiles, setProfiles] = useState<ProfileResponse[]>([])
  const [savingDefault, setSavingDefault] = useState<MonitorField | null>(null)
  // A status read is applied only if nothing was saved after it was asked for.
  const statusRead = useRef(0)

  const loadData = useCallback(async () => {
    try {
      const [monitor, profileList] = await Promise.all([
        api.monitorStatus().catch(() => EMPTY_MONITOR_STATUS),
        // All pages — admin profiles past the first page (limit=100)
        // must not silently vanish from this list.
        api.listAllProfiles(),
      ])
      setMonitorStatus(normalizeStatus(monitor))
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

  const handleToggleDefault = async (field: MonitorField, enabled: boolean) => {
    if (savingDefault) return
    setSavingDefault(field)
    const name = field === 'watchdog' ? 'Watchdog' : 'Alerts'
    try {
      const result =
        field === 'watchdog'
          ? (await api.toggleWatchdog(enabled)).watchdog_enabled
          : (await api.toggleAlerts(enabled)).alerts_enabled
      // Inherited rows follow at once; nothing else is saved.
      setMonitorStatus((prev) => withSystemDefault(prev, field, result))
      // Then show what the server holds: a server with no monitor configured
      // answers the toggle without keeping it.
      const read = ++statusRead.current
      const held = await api.monitorStatus().catch(() => null)
      if (held && read === statusRead.current) {
        const status = normalizeStatus(held)
        setMonitorStatus(status)
        const kept = field === 'watchdog' ? status.watchdog_enabled : status.alerts_enabled
        if (kept !== enabled) {
          toast(
            `The server did not keep this change: ${name.toLowerCase()} is still ${kept ? 'enabled' : 'disabled'}.`,
            'error',
          )
          return
        }
      }
      toast(`${name} ${result ? 'enabled' : 'disabled'}`)
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setSavingDefault(null)
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
      // A status read asked for before this save would show the old row.
      statusRead.current += 1
      // The server computed this row against the default it held then; an
      // inherited field follows the default the page holds now.
      setMonitorStatus((prev) => ({
        ...prev,
        profiles: prev.profiles.map((item) =>
          item.id === updated.id ? inheritDefault(updated, prev) : item,
        ),
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
            busy={savingDefault !== null}
            onChange={(enabled) => handleToggleDefault('watchdog', enabled)}
          />
          <Toggle
            label="Alerts enabled"
            description="Fallback alert behavior for profiles without an override"
            checked={monitorStatus.alerts_enabled}
            busy={savingDefault !== null}
            onChange={(enabled) => handleToggleDefault('alerts', enabled)}
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
                <StatusBadge
                  className="shrink-0"
                  running={profile.status.running}
                  status={profile.status.status}
                  error={profile.status.error ?? null}
                />
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
  busy,
  onChange,
}: {
  label: string
  description: string
  checked: boolean
  busy: boolean
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
        role="switch"
        aria-checked={checked}
        aria-label={label}
        aria-busy={busy}
        disabled={busy}
        onClick={() => onChange(!checked)}
        className={`relative inline-flex h-5 w-9 items-center rounded-full transition-colors disabled:opacity-50 ${
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
