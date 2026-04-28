import { useEffect, useRef, useState } from 'react'
import LlmProviderTab from '../../components/tabs/LlmProviderTab'
import { api } from '../../api'
import type { ProfileConfig } from '../../types'

const ADMIN_PROFILE_ID = 'admin'

const DEFAULT_CONFIG: ProfileConfig = {
  channels: [],
  gateway: {},
  env_vars: {},
}

type SaveState =
  | { kind: 'loading' }
  | { kind: 'idle' }
  | { kind: 'saving' }
  | { kind: 'saved' }
  | { kind: 'err'; error: string }

export default function StepLlmProvider() {
  const [config, setConfig] = useState<ProfileConfig | null>(null)
  const [save, setSave] = useState<SaveState>({ kind: 'loading' })
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Holds the latest unsaved config so unmount/beforeunload can flush it
  // even when the debounce window hasn't elapsed.
  const pendingConfig = useRef<ProfileConfig | null>(null)

  useEffect(() => {
    let cancelled = false
    api
      .getProfile(ADMIN_PROFILE_ID)
      .then((p) => {
        if (cancelled) return
        setConfig({ ...DEFAULT_CONFIG, ...p.config })
        setSave({ kind: 'idle' })
      })
      .catch((e: any) => {
        if (cancelled) return
        console.warn('failed to load admin profile', e)
        setConfig(DEFAULT_CONFIG)
        setSave({ kind: 'err', error: e?.message || 'Could not load admin profile' })
      })
    return () => {
      cancelled = true
    }
  }, [])

  // On unmount and on tab close, fire any pending save instead of dropping it.
  useEffect(() => {
    const flushPending = () => {
      if (saveTimer.current) {
        clearTimeout(saveTimer.current)
        saveTimer.current = null
      }
      const cfg = pendingConfig.current
      if (!cfg) return
      pendingConfig.current = null
      // Fire-and-forget: we may already be unmounting, so the response can't
      // update UI. The data still lands on the server.
      api.updateProfile(ADMIN_PROFILE_ID, { config: cfg }).catch((e: any) => {
        console.warn('flush updateProfile failed', e)
      })
    }
    window.addEventListener('beforeunload', flushPending)
    return () => {
      window.removeEventListener('beforeunload', flushPending)
      flushPending()
    }
  }, [])

  const handleChange = (next: ProfileConfig) => {
    setConfig(next)
    setSave({ kind: 'saving' })
    pendingConfig.current = next
    if (saveTimer.current) clearTimeout(saveTimer.current)
    saveTimer.current = setTimeout(async () => {
      saveTimer.current = null
      pendingConfig.current = null
      try {
        await api.updateProfile(ADMIN_PROFILE_ID, { config: next })
        setSave({ kind: 'saved' })
      } catch (e: any) {
        setSave({ kind: 'err', error: e?.message || 'Save failed' })
      }
    }, 500)
  }

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">LLM Provider</h2>
        <p className="text-sm text-gray-400">
          Configure the LLM for your admin profile. Changes are saved automatically to the
          admin profile (also editable later under Profile → LLM).
        </p>
      </div>

      {config ? (
        <LlmProviderTab
          config={config}
          onChange={handleChange}
          profileId={ADMIN_PROFILE_ID}
        />
      ) : (
        <div className="text-sm text-gray-500">Loading admin profile…</div>
      )}

      <div className="text-xs">
        {save.kind === 'loading' && <span className="text-gray-400">Loading…</span>}
        {save.kind === 'saving' && <span className="text-gray-400">Saving…</span>}
        {save.kind === 'saved' && <span className="text-green-400">Saved</span>}
        {save.kind === 'err' && (
          <span className="text-red-400 break-all">✗ {save.error}</span>
        )}
      </div>
    </div>
  )
}
