import { Link, Navigate } from 'react-router-dom'
import { useOverview } from '../hooks/useProfiles'
import { useToast } from '../components/Toast'
import { useAuth } from '../contexts/AuthContext'
import ProfileCard from '../components/ProfileCard'
import { api } from '../api'
import { useState, useMemo } from 'react'
import type { ProfileResponse } from '../types'

const HIDDEN_DASHBOARD_APP_IDS = new Set(['studio', 'slidesstudio'])

function dashboardAppKey(value: string | null | undefined) {
  return (value || '').toLowerCase().replace(/[\s_-]+/g, '')
}

function shouldShowDashboardApp(profile: ProfileResponse) {
  return !(
    HIDDEN_DASHBOARD_APP_IDS.has(dashboardAppKey(profile.id)) ||
    HIDDEN_DASHBOARD_APP_IDS.has(dashboardAppKey(profile.name))
  )
}

export default function Dashboard() {
  const { isAdmin } = useAuth()
  const { data, error, loading, refresh } = useOverview()
  const { toast } = useToast()
  const [actionLoading, setActionLoading] = useState(false)

  // Partition profiles into parents and sub-accounts (must be before early returns)
  const { parentProfiles, subAccountMap, totalSubAccounts, visibleRunning, visibleStopped } = useMemo(() => {
    const all = (data?.profiles || []).filter(shouldShowDashboardApp)
    const parents = all.filter(p => !p.parent_id)
    const map = new Map<string, ProfileResponse[]>()
    let subCount = 0
    for (const p of all) {
      if (p.parent_id) {
        subCount++
        const subs = map.get(p.parent_id) || []
        subs.push(p)
        map.set(p.parent_id, subs)
      }
    }
    return {
      parentProfiles: parents,
      subAccountMap: map,
      totalSubAccounts: subCount,
      visibleRunning: all.filter(p => p.status.running).length,
      visibleStopped: all.filter(p => !p.status.running).length,
    }
  }, [data?.profiles])

  // Non-admins go straight to their profile
  if (!isAdmin) {
    return <Navigate to="/my" replace />
  }

  const handleStart = async (id: string) => {
    try {
      setActionLoading(true)
      await api.startGateway(id)
      toast(`Gateway '${id}' started`)
      refresh()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setActionLoading(false)
    }
  }

  const handleStop = async (id: string) => {
    try {
      setActionLoading(true)
      await api.stopGateway(id)
      toast(`Gateway '${id}' stopped`)
      refresh()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setActionLoading(false)
    }
  }

  const handleStartAll = async () => {
    try {
      setActionLoading(true)
      const res = await api.startAll()
      toast(`Started ${res.count} gateways`)
      refresh()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setActionLoading(false)
    }
  }

  const handleStopAll = async () => {
    try {
      setActionLoading(true)
      const res = await api.stopAll()
      toast(`Stopped ${res.count} gateways`)
      refresh()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setActionLoading(false)
    }
  }

  if (loading && !data) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  if (error) {
    return (
      <div className="bg-red-500/10 border border-red-500/30 rounded-lg p-4 text-red-400 text-sm">
        Failed to load profiles: {error}
      </div>
    )
  }

  return (
    <div>
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-white">Dashboard</h1>
          <p className="text-sm text-gray-500 mt-1">
            {parentProfiles.length} profiles
            {totalSubAccounts > 0 && (
              <span className="text-gray-600 ml-1">({totalSubAccounts} sub-accounts)</span>
            )}
            {visibleRunning > 0 && (
              <span className="text-green-400 ml-2">{visibleRunning} running</span>
            )}
          </p>
        </div>
        <div className="flex gap-2">
          {visibleRunning > 0 && (
            <button
              onClick={handleStopAll}
              disabled={actionLoading}
              className="px-4 py-2 text-sm font-medium rounded-lg bg-red-500/10 text-red-400 hover:bg-red-500/20 border border-red-500/20 transition disabled:opacity-50"
            >
              Stop All
            </button>
          )}
          {visibleStopped > 0 && (
            <button
              onClick={handleStartAll}
              disabled={actionLoading}
              className="px-4 py-2 text-sm font-medium rounded-lg bg-green-500/10 text-green-400 hover:bg-green-500/20 border border-green-500/20 transition disabled:opacity-50"
            >
              Start All
            </button>
          )}
          <Link
            to="/profiles/new"
            className="px-4 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
          >
            + New Profile
          </Link>
        </div>
      </div>

      {/* Profile grid */}
      {parentProfiles.length > 0 ? (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
          {parentProfiles.map((p) => (
            <ProfileCard
              key={p.id}
              profile={p}
              subAccounts={subAccountMap.get(p.id)}
              onStart={handleStart}
              onStop={handleStop}
            />
          ))}
        </div>
      ) : (
        <div className="text-center py-16">
          <div className="text-gray-600 text-5xl mb-4">+</div>
          <h3 className="text-lg font-medium text-gray-400 mb-2">No profiles yet</h3>
          <p className="text-sm text-gray-500 mb-4">
            Create a profile to get started with multi-user gateway management.
          </p>
          <Link
            to="/profiles/new"
            className="inline-flex px-6 py-2.5 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
          >
            Create First Profile
          </Link>
        </div>
      )}
    </div>
  )
}
