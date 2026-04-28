import { useEffect, useState } from 'react'
import { api, type DeploymentMode } from '../../api'

const INSTALL_URL =
  'https://github.com/octos-org/octos/releases/latest/download/install.sh'

type Guidance = {
  tone: 'ok' | 'info' | 'warn'
  title: string
  body: string
  command?: string
  commandNote?: string
}

function guidanceFor(
  selected: DeploymentMode,
  detected: DeploymentMode | null,
): Guidance {
  // Selected matches detected → host already configured for this mode.
  if (detected && selected === detected) {
    if (selected === 'local')
      return {
        tone: 'ok',
        title: 'Local mode — host is ready',
        body: 'Standalone install. octos serve runs on 127.0.0.1; no tunnel, no public landing page. Nothing else to do.',
      }
    if (selected === 'tenant')
      return {
        tone: 'ok',
        title: 'Tenant mode — frpc tunnel detected',
        body: 'frpc.toml was found and the host is set up to tunnel out to your cloud relay. No further action needed; restart octos serve if you changed the mode.',
      }
    return {
      tone: 'ok',
      title: 'Cloud mode — VPS relay environment detected',
      body: 'TUNNEL_DOMAIN is set and this host is provisioned as the relay. Tenants can register through this node. Restart octos serve if you changed the mode.',
    }
  }

  // Overrides.
  if (selected === 'tenant')
    return {
      tone: 'info',
      title: 'Tenant mode needs a token issued by your cloud operator',
      body:
        "Tunnel auth is per-tenant — there is no shared operator-wide token. Your cloud operator registers you in their tenant store and issues a unique token (written to frpc's metadatas.token). Don't make one up; ask the operator to register you on their /admin/tenants page.",
      command: `curl -fsSL ${INSTALL_URL} | sudo bash -s -- \\
    --tunnel \\
    --tenant-name <YOUR_NAME> \\
    --frps-token <PER_TENANT_TOKEN> \\
    --frps-server <FRPS_HOST> \\
    --domain <CLOUD_HOST>`,
      commandNote:
        'The operator portal returns the exact one-liner for you (with all four values pre-filled). Prefer pasting that — this template is a fallback. Without a registered token, frps will refuse the tunnel.',
    }

  if (selected === 'cloud')
    return {
      tone: 'warn',
      title: 'Cloud mode means this node IS the relay',
      body:
        "Pick this only on a public VPS with DNS pointed at it. You'll terminate tunnels from tenants, run frps, and serve the public landing page. Skip otherwise — picking cloud on a laptop won't work, the saved value just controls server behavior.",
      command:
        'sudo bash scripts/cloud-host-deploy.sh --domain <YOUR_DOMAIN>',
      commandNote:
        'Run on the VPS itself (not over the wizard on a laptop). The script installs frps + Caddy and writes config.json with mode = "cloud" and TUNNEL_DOMAIN set.',
    }

  // selected === 'local' but detected something else.
  if (detected === 'tenant')
    return {
      tone: 'info',
      title: 'Switching to local will leave the frpc tunnel running',
      body:
        'The mode flag only changes how octos serve behaves. The frpc client is a separate process that keeps tunneling traffic until you stop it.',
      command:
        '# macOS\nsudo launchctl unload /Library/LaunchDaemons/io.octos.frpc.plist\n# Linux\nsudo systemctl disable --now frpc',
      commandNote:
        'Or run sudo bash scripts/install.sh --uninstall to remove the binary, frpc, and all related config.',
    }

  if (detected === 'cloud')
    return {
      tone: 'warn',
      title: 'Switching cloud → local on a VPS is unusual',
      body:
        'This host has TUNNEL_DOMAIN set and is registered as the relay for tenants. Switching to local stops serving the landing page and the frps plugin endpoints, but tenants pointed at this host will keep trying to connect.',
      commandNote:
        'If you actually want to retire this VPS, plan a migration window first; tenants need to be re-pointed at a new relay.',
    }

  return {
    tone: 'ok',
    title: 'Local mode',
    body: 'Standalone install. octos serve runs on 127.0.0.1; no tunnel.',
  }
}

const TONE_CLASS: Record<Guidance['tone'], string> = {
  ok: 'border-emerald-500/40 bg-emerald-500/5',
  info: 'border-sky-500/40 bg-sky-500/5',
  warn: 'border-amber-500/40 bg-amber-500/5',
}

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
  const [explicit, setExplicit] = useState(false)
  const [save, setSave] = useState<SaveState>({ kind: 'idle' })

  useEffect(() => {
    let cancelled = false
    Promise.all([api.getDeploymentMode(), api.detectDeploymentMode()])
      .then(([current, detection]) => {
        if (cancelled) return
        const detectedMode = detection.detected
        setDetected(detectedMode)
        // Auto-apply detection only when the user has not made an explicit
        // choice yet (mode field absent from config.json). When `explicit` is
        // true, respect their pick — never silently overwrite it on revisit,
        // even if detection now disagrees.
        if (!current.explicit && detectedMode && detectedMode !== current.mode) {
          setMode(detectedMode)
          setExplicit(true)
          setSave({ kind: 'saving' })
          api
            .saveDeploymentMode(detectedMode)
            .then(() => {
              if (cancelled) return
              setSave({ kind: 'saved' })
              onModeSaved(detectedMode)
            })
            .catch((e: any) => {
              if (cancelled) return
              setSave({ kind: 'err', error: e?.message || 'Save failed.' })
            })
        } else {
          setMode(current.mode)
          setExplicit(Boolean(current.explicit))
        }
      })
      .catch((e) => {
        if (cancelled) return
        console.warn('getDeploymentMode failed', e)
        setMode('local')
      })
    return () => {
      cancelled = true
    }
  }, [onModeSaved])

  const handleSelect = async (next: DeploymentMode) => {
    setMode(next)
    setExplicit(true)
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
        {detected && (
          <p className="text-xs text-gray-500 mt-1">
            {explicit && mode && detected !== mode ? (
              <>
                Detection suggests <span className="font-mono text-gray-300">{detected}</span>,
                but you previously chose <span className="font-mono text-gray-300">{mode}</span>.
                Keeping your pick — click any option to change.
              </>
            ) : (
              <>
                Auto-detected as <span className="font-mono text-gray-300">{detected}</span> from this host's environment{explicit ? ' — matches your saved choice.' : ' — pre-selected. Pick another option to override.'}
              </>
            )}
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

      {mode && <GuidancePanel guidance={guidanceFor(mode, detected)} />}
    </div>
  )
}

function GuidancePanel({ guidance }: { guidance: Guidance }) {
  const [copied, setCopied] = useState(false)

  const onCopy = async () => {
    if (!guidance.command) return
    try {
      await navigator.clipboard.writeText(guidance.command)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      /* ignore */
    }
  }

  return (
    <div className={`mt-2 rounded-lg border p-3 ${TONE_CLASS[guidance.tone]}`}>
      <div className="text-sm font-medium text-white">{guidance.title}</div>
      <div className="text-xs text-gray-300 mt-1">{guidance.body}</div>
      {guidance.command && (
        <div className="mt-2">
          <div className="flex items-center justify-between mb-1">
            <span className="text-[11px] uppercase tracking-wide text-gray-400">
              Run on the host
            </span>
            <button
              type="button"
              onClick={onCopy}
              className="text-[11px] text-gray-300 hover:text-white"
            >
              {copied ? 'Copied' : 'Copy'}
            </button>
          </div>
          <pre className="text-xs bg-black/40 border border-gray-700/50 rounded p-2 whitespace-pre-wrap break-all font-mono text-gray-200">
            {guidance.command}
          </pre>
        </div>
      )}
      {guidance.commandNote && (
        <div className="text-[11px] text-gray-400 mt-2">{guidance.commandNote}</div>
      )}
    </div>
  )
}
