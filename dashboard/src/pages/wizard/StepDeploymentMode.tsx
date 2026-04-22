import { useEffect, useState } from 'react'
import { api, type DeploymentMode } from '../../api'

type SaveState =
  | { kind: 'idle' }
  | { kind: 'saving' }
  | { kind: 'saved' }
  | { kind: 'err'; error: string }

type Props = {
  /** Notified each time the selected mode is saved. */
  onModeSaved: (mode: DeploymentMode) => void
}

const MODES: { id: DeploymentMode; label: string; description: string }[] = [
  {
    id: 'local',
    label: 'Local',
    description:
      'Standalone install — no tunnel, dashboard served at /admin/. Right for first-time users on a laptop or workstation.',
  },
  {
    id: 'tenant',
    label: 'Tenant',
    description:
      'Joins a central VPS via frpc tunnel. Right for personal Mac minis / servers that need a stable public URL via an operator-hosted cloud host.',
  },
  {
    id: 'cloud',
    label: 'Cloud',
    description:
      'This node IS the VPS relay — terminates tunnels from tenants and issues subdomains. Right for the operator-hosted octos-cloud.org host.',
  },
]

export default function StepDeploymentMode({ onModeSaved }: Props) {
  const [mode, setMode] = useState<DeploymentMode | null>(null)
  const [detected, setDetected] = useState<DeploymentMode | null>(null)
  const [save, setSave] = useState<SaveState>({ kind: 'idle' })

  useEffect(() => {
    let cancelled = false
    Promise.all([api.getDeploymentMode(), api.detectDeploymentMode()])
      .then(([current, detection]) => {
        if (cancelled) return
        setMode(current.mode)
        setDetected(detection.detected)
      })
      .catch((e) => {
        if (cancelled) return
        console.warn('getDeploymentMode failed', e)
        setMode('local')
      })
    return () => {
      cancelled = true
    }
  }, [])

  const handleSelect = async (next: DeploymentMode) => {
    setMode(next)
    setSave({ kind: 'saving' })
    try {
      await api.saveDeploymentMode(next)
      setSave({ kind: 'saved' })
      onModeSaved(next)
    } catch (e: any) {
      setSave({ kind: 'err', error: e?.message || 'Save failed.' })
    }
  }

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Deployment Mode</h2>
        <p className="text-sm text-gray-400">
          Choose how this node runs. Your selection is saved immediately.
        </p>
        {detected && mode !== null && detected !== mode && (
          <p className="text-xs text-gray-500 mt-1">
            Auto-detection suggests <span className="font-mono text-gray-300">{detected}</span> based on this host's environment.
          </p>
        )}
      </div>

      <div className="space-y-2">
        {MODES.map((m) => {
          const selected = mode === m.id
          return (
            <label
              key={m.id}
              className={`flex items-start gap-3 p-3 rounded-lg border cursor-pointer transition ${
                selected
                  ? 'border-accent bg-accent/10'
                  : 'border-gray-700/50 bg-background/60 hover:border-gray-500'
              }`}
            >
              <input
                type="radio"
                name="deployment-mode"
                value={m.id}
                checked={selected}
                onChange={() => handleSelect(m.id)}
                className="mt-1"
              />
              <div className="flex-1">
                <div className="text-sm font-medium text-white">
                  {m.label}
                  {detected === m.id && (
                    <span className="ml-2 text-[10px] uppercase tracking-wide text-gray-400">
                      detected
                    </span>
                  )}
                </div>
                <div className="text-xs text-gray-400 mt-0.5">{m.description}</div>
              </div>
            </label>
          )
        })}
      </div>

      <div className="text-xs">
        {save.kind === 'saving' && <span className="text-gray-400">Saving…</span>}
        {save.kind === 'saved' && <span className="text-green-400">Saved</span>}
        {save.kind === 'err' && <span className="text-red-400 break-all">✗ {save.error}</span>}
      </div>
    </div>
  )
}
