import { createContext, useContext, useState, useEffect, useCallback, useMemo, type ReactNode } from 'react'
import { useParams } from 'react-router-dom'
import { useAuth } from './AuthContext'
import { useToast } from '../components/Toast'
import { api, myApi, getLogStreamUrl, getAdminLogStreamUrl } from '../api'
import type { ProfileConfig, ProcessStatus, PurgeReport } from '../types'

const defaultConfig: ProfileConfig = {
  provider: 'anthropic',
  model: 'claude-sonnet-4-20250514',
  api_key_env: 'ANTHROPIC_API_KEY',
  fallback_models: [],
  channels: [],
  gateway: { max_history: 50 },
  env_vars: {},
}

interface ProfileContextValue {
  profileId: string
  parentId: string | null
  config: ProfileConfig
  status: ProcessStatus | null
  isOwn: boolean
  loading: boolean
  saving: boolean
  setConfig: (config: ProfileConfig) => void
  save: () => Promise<void>
  refresh: () => Promise<void>
  startGateway: () => Promise<void>
  stopGateway: () => Promise<void>
  restartGateway: () => Promise<void>
  profileName: string
  setProfileName: (name: string) => void
  profileEmail: string
  setProfileEmail: (email: string) => void
  publicSubdomain: string
  setPublicSubdomain: (subdomain: string) => void
  enabled: boolean
  setEnabled: (enabled: boolean) => void
  logStreamUrl: string
  deleteProfile: () => Promise<void>
  purgeProfile: () => Promise<PurgeReport | null>
}

const ProfileContext = createContext<ProfileContextValue | null>(null)

interface Props {
  children: ReactNode
}

export function ProfileProvider({ children }: Props) {
  const { id } = useParams<{ id: string }>()
  const { user, isAdmin } = useAuth()
  const { toast } = useToast()

  // Determine if viewing own profile
  const isOwn = !id
  const profileId = id || user?.id || ''

  const [config, setConfig] = useState<ProfileConfig>(defaultConfig)
  const [status, setStatus] = useState<ProcessStatus | null>(null)
  const [profileName, setProfileName] = useState('')
  const [profileEmail, setProfileEmail] = useState('')
  const [publicSubdomain, setPublicSubdomain] = useState('')
  const [enabled, setEnabled] = useState(true)
  const [parentId, setParentId] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)

  // API adapter: own profile uses myApi, admin uses api with profileId
  const adapter = useMemo(() => {
    if (isOwn) {
      return {
        getProfile: () => myApi.getProfile(),
        updateProfile: (data: any) => myApi.updateProfile(data),
        startGateway: () => myApi.startGateway(),
        stopGateway: () => myApi.stopGateway(),
        restartGateway: () => myApi.restartGateway(),
      }
    }
    if (!isAdmin) {
      return {
        getProfile: () => myApi.getSubAccount(profileId),
        updateProfile: (data: any) => myApi.updateSubAccount(profileId, data),
        startGateway: () => myApi.startSubGateway(profileId),
        stopGateway: () => myApi.stopSubGateway(profileId),
        restartGateway: async () => {
          await myApi.stopSubGateway(profileId)
          await myApi.startSubGateway(profileId)
        },
      }
    }
    return {
      getProfile: () => api.getProfile(profileId),
      updateProfile: (data: any) => api.updateProfile(profileId, data),
      startGateway: () => api.startGateway(profileId),
      stopGateway: () => api.stopGateway(profileId),
      restartGateway: () => api.restartGateway(profileId),
    }
  }, [isAdmin, isOwn, profileId])

  const logStreamUrl = useMemo(
    () => (isOwn ? getLogStreamUrl() : getAdminLogStreamUrl(profileId)),
    [isOwn, profileId],
  )

  const loadProfile = useCallback(async () => {
    try {
      setLoading(true)
      const profile = await adapter.getProfile()
      setConfig(profile.config)
      setStatus(profile.status)
      setProfileName(profile.name)
      setProfileEmail(profile.email || '')
      setPublicSubdomain(profile.public_subdomain || profile.id)
      setEnabled(profile.enabled)
      setParentId(profile.parent_id || null)
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setLoading(false)
    }
  }, [adapter, toast])

  useEffect(() => {
    loadProfile()
  }, [loadProfile])

  const save = useCallback(async () => {
    try {
      setSaving(true)
      const profile = await adapter.updateProfile({
        name: profileName,
        email: profileEmail || undefined,
        public_subdomain:
          publicSubdomain.trim() && publicSubdomain.trim() !== profileId
            ? publicSubdomain.trim()
            : null,
        enabled,
        config,
      })
      setConfig(profile.config)
      setStatus(profile.status)
      setProfileName(profile.name)
      setPublicSubdomain(profile.public_subdomain || profile.id)
      setEnabled(profile.enabled)
      toast('Configuration saved')
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setSaving(false)
    }
  }, [adapter, config, profileEmail, profileId, profileName, publicSubdomain, enabled, toast])

  const startGateway = useCallback(async () => {
    try {
      await adapter.startGateway()
      toast('Gateway started')
      await loadProfile()
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }, [adapter, loadProfile, toast])

  const stopGateway = useCallback(async () => {
    try {
      await adapter.stopGateway()
      toast('Gateway stopped')
      await loadProfile()
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }, [adapter, loadProfile, toast])

  const restartGateway = useCallback(async () => {
    try {
      await adapter.restartGateway()
      toast('Gateway restarted')
      await loadProfile()
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }, [adapter, loadProfile, toast])

  const deleteProfile = useCallback(async () => {
    if (isOwn) return
    try {
      await api.deleteProfile(profileId)
      toast('Profile deleted')
    } catch (e: any) {
      toast(e.message, 'error')
    }
  }, [isOwn, profileId, toast])

  const purgeProfile = useCallback(async (): Promise<PurgeReport | null> => {
    if (isOwn) return null
    try {
      const report = await api.purgeProfile(profileId)
      const mb = (report.bytes_freed / 1024 / 1024).toFixed(1)
      toast(`Purged: freed ${mb} MB${report.port_released != null ? `, released port ${report.port_released}` : ''}`)
      return report
    } catch (e: any) {
      toast(e.message, 'error')
      return null
    }
  }, [isOwn, profileId, toast])

  const value: ProfileContextValue = {
    profileId,
    parentId,
    config,
    status,
    isOwn,
    loading,
    saving,
    setConfig,
    save,
    refresh: loadProfile,
    startGateway,
    stopGateway,
    restartGateway,
    profileName,
    setProfileName,
    profileEmail,
    setProfileEmail,
    publicSubdomain,
    setPublicSubdomain,
    enabled,
    setEnabled,
    logStreamUrl,
    deleteProfile,
    purgeProfile,
  }

  return <ProfileContext.Provider value={value}>{children}</ProfileContext.Provider>
}

export function useProfile(): ProfileContextValue {
  const ctx = useContext(ProfileContext)
  if (!ctx) throw new Error('useProfile must be used within ProfileProvider')
  return ctx
}
