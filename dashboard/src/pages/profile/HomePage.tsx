import { useState, useEffect, useCallback } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { useProfile } from '../../contexts/ProfileContext'
import { useAuth } from '../../contexts/AuthContext'
import GatewayControls from '../../components/GatewayControls'
import ConfirmDialog from '../../components/ConfirmDialog'
import StatusBadge from '../../components/StatusBadge'
import { CHANNEL_COLORS, CHANNEL_LABELS } from '../../types'
import type { ProfileResponse } from '../../types'
import { api, myApi } from '../../api'
import { useToast } from '../../components/Toast'

export default function HomePage() {
  const { isAdmin } = useAuth()
  const {
    profileId, parentId, config, setConfig, status, isOwn, loading,
    startGateway, stopGateway, restartGateway,
    profileName, setProfileName, profileEmail, setProfileEmail, publicSubdomain, setPublicSubdomain, enabled, setEnabled,
    save, saving, deleteProfile,
  } = useProfile()
  const navigate = useNavigate()
  const { toast } = useToast()
  const [actionLoading, setActionLoading] = useState(false)
  const [deleteOpen, setDeleteOpen] = useState(false)

  // Sub-accounts state
  const [subAccounts, setSubAccounts] = useState<ProfileResponse[]>([])
  const [subsLoading, setSubsLoading] = useState(false)
  const [showCreateSub, setShowCreateSub] = useState(false)
  const [newSubId, setNewSubId] = useState('')
  const [newSubName, setNewSubName] = useState('')
  const [newSubdomain, setNewSubdomain] = useState('')
  const [newSubEmail, setNewSubEmail] = useState('')
  const [createSubLoading, setCreateSubLoading] = useState(false)

  const loadSubAccounts = useCallback(async () => {
    if (parentId || !profileId) return
    try {
      setSubsLoading(true)
      const subs = isAdmin
        ? await api.listSubAccounts(profileId)
        : await myApi.listSubAccounts()
      setSubAccounts(subs)
    } catch {
      // silently ignore — profile may not have sub-accounts
    } finally {
      setSubsLoading(false)
    }
  }, [profileId, parentId, isAdmin])

  useEffect(() => {
    loadSubAccounts()
  }, [loadSubAccounts])

  const handleCreateSubAccount = async () => {
    if (!profileId || !newSubName.trim()) return
    setCreateSubLoading(true)
    try {
      const payload = {
        sub_account_id: newSubId.trim() || undefined,
        name: newSubName.trim(),
        public_subdomain: newSubdomain.trim() || undefined,
        email: newSubEmail.trim() || undefined,
      }
      if (isAdmin) {
        await api.createSubAccount(profileId, payload)
      } else {
        await myApi.createSubAccount(payload)
      }
      toast('Sub-account created', 'success')
      setNewSubId('')
      setNewSubName('')
      setNewSubdomain('')
      setNewSubEmail('')
      setShowCreateSub(false)
      loadSubAccounts()
    } catch (e: any) {
      toast(e.message || 'Failed to create sub-account', 'error')
    } finally {
      setCreateSubLoading(false)
    }
  }

  const handleStart = async () => {
    setActionLoading(true)
    await startGateway()
    setActionLoading(false)
  }
  const handleStop = async () => {
    setActionLoading(true)
    await stopGateway()
    setActionLoading(false)
  }
  const handleRestart = async () => {
    setActionLoading(true)
    await restartGateway()
    setActionLoading(false)
  }
  const handleDelete = async () => {
    await deleteProfile()
    setDeleteOpen(false)
    navigate('/')
  }

  const handleSubStart = async (id: string) => {
    try {
      if (isAdmin) {
        await api.startGateway(id)
      } else {
        await myApi.startSubGateway(id)
      }
      toast(`Gateway '${id}' started`)
      loadSubAccounts()
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }

  const handleSubStop = async (id: string) => {
    try {
      if (isAdmin) {
        await api.stopGateway(id)
      } else {
        await myApi.stopSubGateway(id)
      }
      toast(`Gateway '${id}' stopped`)
      loadSubAccounts()
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

  const channels = config.channels || []
  const needsSetup = !config.provider && !config.model

  return (
    <div>
      <h1 className="text-2xl font-bold text-white mb-6">Overview</h1>

      {needsSetup && (
        <div className="mb-6 bg-amber-500/10 border border-amber-500/30 rounded-xl p-5">
          <h3 className="text-sm font-semibold text-amber-300 mb-2">Setup Required</h3>
          <p className="text-sm text-amber-200/80 mb-3">
            This profile hasn't been configured yet. Set up an LLM provider to get started.
          </p>
          <Link
            to={`${isOwn ? '/my' : `/profile/${profileId}`}/llm`}
            className="inline-flex px-4 py-2 text-sm font-medium rounded-lg bg-amber-500/20 text-amber-300 hover:bg-amber-500/30 border border-amber-500/30 transition"
          >
            Configure LLM Provider
          </Link>
        </div>
      )}

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* Gateway Controls */}
        <GatewayControls
          status={status || { running: false, pid: null, started_at: null, uptime_secs: null }}
          loading={actionLoading}
          onStart={handleStart}
          onStop={handleStop}
          onRestart={handleRestart}
        />

        {/* Profile Info */}
        <div className="bg-surface rounded-xl border border-gray-700/50 p-5">
          <h3 className="text-sm font-semibold text-white mb-4">Profile Info</h3>
          <dl className="space-y-3 text-xs">
            <InfoRow label="ID" value={profileId} />
            <InfoRow label="Public URL ID" value={publicSubdomain || profileId} />
            {parentId && (
              <div className="flex justify-between">
                <dt className="text-gray-500">Parent</dt>
                <dd>
                  <Link
                    to={`/profile/${parentId}`}
                    className="text-accent hover:text-accent-light transition-colors"
                  >
                    {parentId}
                  </Link>
                </dd>
              </div>
            )}
            <InfoRow label="Provider" value={config.provider || 'anthropic'} />
            <InfoRow label="Model" value={config.model || 'default'} />
            <InfoRow
              label="Channels"
              value={channels.length > 0 ? channels.map((c) => c.type).join(', ') : 'None'}
            />
            <InfoRow label="Fallbacks" value={String(config.fallback_models?.length || 0)} />
          </dl>

          {channels.length > 0 && (
            <div className="flex flex-wrap gap-1.5 mt-4">
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
        </div>
      </div>

      {/* Profile Settings */}
      <div className="mt-6 bg-surface rounded-xl border border-gray-700/50 p-5">
        <h3 className="text-sm font-semibold text-white mb-4">Profile Settings</h3>
        <div className="space-y-4">
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Display Name</label>
            <input
              value={profileName}
              onChange={(e) => setProfileName(e.target.value)}
              className="input max-w-md"
            />
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Public Subdomain</label>
            <input
              value={publicSubdomain}
              onChange={(e) => setPublicSubdomain(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
              placeholder={profileId}
              className="input max-w-md"
              disabled={!!parentId && isOwn}
            />
            <p className="text-xs text-gray-500 mt-1">
              Public URL preview: <span className="font-mono text-gray-400">https://{(publicSubdomain || profileId) || 'your-subdomain'}.crew.ominix.io</span>
            </p>
            {!!parentId && isOwn && (
              <p className="text-xs text-gray-500 mt-1">Sub-account users cannot change their own public subdomain. The parent account controls it.</p>
            )}
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Login Email</label>
            <input
              value={profileEmail}
              onChange={(e) => setProfileEmail(e.target.value)}
              placeholder="user@example.com (for web client OTP login)"
              type="email"
              className="input max-w-md"
            />
            <p className="text-xs text-gray-500 mt-1">Set an email address to enable OTP login to the web client for this profile.</p>
          </div>
          <div>
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="checkbox"
                checked={enabled}
                onChange={(e) => setEnabled(e.target.checked)}
                className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
              />
              <span className="text-sm text-gray-400">Auto-start gateway when server starts</span>
            </label>
          </div>
          {isAdmin && !isOwn && (
            <div>
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="checkbox"
                  checked={config.admin_mode || false}
                  onChange={(e) => setConfig({ ...config, admin_mode: e.target.checked })}
                  className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
                />
                <span className="text-sm text-gray-400">Admin mode (admin-only tools, no shell/file/web)</span>
              </label>
            </div>
          )}
          <div className="flex gap-3 pt-2">
            <button
              onClick={save}
              disabled={saving}
              className="px-5 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
            >
              {saving ? 'Saving...' : 'Save'}
            </button>
            {isAdmin && !isOwn && (
              <button
                onClick={() => setDeleteOpen(true)}
                className="px-4 py-2 text-sm font-medium rounded-lg bg-red-500/10 text-red-400 hover:bg-red-500/20 border border-red-500/20 transition"
              >
                Delete Profile
              </button>
            )}
          </div>
        </div>
      </div>

      {/* Sub-Accounts section — only for parent profiles */}
      {!parentId && (
        <div className="mt-6 bg-surface rounded-xl border border-gray-700/50 p-5">
          <div className="flex items-center justify-between mb-4">
            <h3 className="text-sm font-semibold text-white">
              Sub-Accounts
              {subAccounts.length > 0 && (
                <span className="ml-2 text-gray-500 font-normal">({subAccounts.length})</span>
              )}
            </h3>
            {(
              <button
                onClick={() => setShowCreateSub(!showCreateSub)}
                className="px-3 py-1.5 text-xs font-medium rounded-lg bg-accent/10 text-accent hover:bg-accent/20 transition"
              >
                {showCreateSub ? 'Cancel' : '+ Add'}
              </button>
            )}
          </div>

          {showCreateSub && (
            <div className="mb-4 p-3 rounded-lg bg-white/[0.03] border border-gray-700/50 space-y-3">
              <div>
                <label className="block text-xs text-gray-400 mb-1">User ID</label>
                <input
                  value={newSubId}
                  onChange={e => setNewSubId(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
                  placeholder="newsbot"
                  className="w-full bg-white/5 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-accent"
                />
                <p className="text-[11px] text-gray-500 mt-1">One-time internal child ID. Final internal profile ID becomes <span className="font-mono">{profileId || 'parent'}--{newSubId || 'child-id'}</span>.</p>
              </div>
              <div>
                <label className="block text-xs text-gray-400 mb-1">Name</label>
                <input
                  value={newSubName}
                  onChange={e => setNewSubName(e.target.value)}
                  placeholder="e.g. work-bot"
                  className="w-full bg-white/5 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-accent"
                />
              </div>
              <div>
                <label className="block text-xs text-gray-400 mb-1">Public Subdomain</label>
                <input
                  value={newSubdomain}
                  onChange={e => setNewSubdomain(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
                  placeholder="newsbot"
                  className="w-full bg-white/5 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-accent"
                />
                <p className="text-[11px] text-gray-500 mt-1">Public URL: <span className="font-mono">https://{newSubdomain || 'newsbot'}.crew.ominix.io</span></p>
              </div>
              <div>
                <label className="block text-xs text-gray-400 mb-1">Email (for web client login)</label>
                <input
                  value={newSubEmail}
                  onChange={e => setNewSubEmail(e.target.value)}
                  placeholder="user@example.com"
                  type="email"
                  className="w-full bg-white/5 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-accent"
                />
              </div>
              <button
                onClick={handleCreateSubAccount}
                disabled={
                  createSubLoading
                    || !newSubName.trim()
                    || !newSubId.trim()
                    || !newSubdomain.trim()
                }
                className="px-4 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent/80 transition disabled:opacity-50"
              >
                {createSubLoading ? 'Creating...' : 'Create Sub-Account'}
              </button>
            </div>
          )}

          {subsLoading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin w-5 h-5 border-2 border-accent border-t-transparent rounded-full" />
            </div>
          ) : subAccounts.length > 0 ? (
            <div className="space-y-2">
              {subAccounts.map((sub) => {
                const subChannels = sub.config.channels || []
                const shortName = sub.name

                return (
                  <div
                    key={sub.id}
                    className="flex items-center gap-3 py-2.5 px-3 rounded-lg bg-white/[0.02] hover:bg-white/[0.04] transition-colors"
                  >
                    <StatusBadge running={sub.status.running} className="shrink-0" />

                    <Link
                      to={`/profile/${sub.id}`}
                      className="text-sm text-gray-300 hover:text-accent transition-colors truncate min-w-0 flex-1 font-medium"
                    >
                      {shortName}
                    </Link>

                    {sub.status.running && sub.status.uptime_secs && (
                      <span className="text-xs text-gray-500 shrink-0">
                        {formatUptime(sub.status.uptime_secs)}
                      </span>
                    )}

                    {subChannels.length > 0 && (
                      <div className="flex gap-1 shrink-0">
                        {subChannels.map((ch, i) => {
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

                    <div className="flex gap-1.5 shrink-0">
                      {sub.status.running ? (
                        <button
                          onClick={() => handleSubStop(sub.id)}
                          className="px-2.5 py-1 text-xs font-medium rounded-lg bg-red-500/10 text-red-400 hover:bg-red-500/20 transition"
                        >
                          Stop
                        </button>
                      ) : (
                        <button
                          onClick={() => handleSubStart(sub.id)}
                          className="px-2.5 py-1 text-xs font-medium rounded-lg bg-green-500/10 text-green-400 hover:bg-green-500/20 transition"
                        >
                          Start
                        </button>
                      )}
                      <Link
                        to={`/profile/${sub.id}`}
                        className="px-2.5 py-1 text-xs font-medium rounded-lg bg-white/5 text-gray-400 hover:bg-white/10 hover:text-white transition"
                      >
                        Configure
                      </Link>
                    </div>
                  </div>
                )
              })}
            </div>
          ) : (
            <p className="text-sm text-gray-500 py-4 text-center">No sub-accounts</p>
          )}
        </div>
      )}

      <ConfirmDialog
        open={deleteOpen}
        title="Delete Profile"
        message={`Are you sure you want to delete "${profileName}"? This will stop the gateway and remove all configuration.`}
        confirmLabel="Delete"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeleteOpen(false)}
      />
    </div>
  )
}

function InfoRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between">
      <dt className="text-gray-500">{label}</dt>
      <dd className="text-gray-300">{value}</dd>
    </div>
  )
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
