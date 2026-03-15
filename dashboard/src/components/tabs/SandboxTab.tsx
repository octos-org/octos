import type { ProfileConfig, SandboxConfig, DockerConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

const MODES = [
  { value: 'auto', label: 'Auto', desc: 'macOS sandbox-exec on Mac, Docker on Linux' },
  { value: 'macos', label: 'macOS (sandbox-exec)', desc: 'Kernel-level SBPL, ~6ms overhead' },
  { value: 'docker', label: 'Docker', desc: 'Container isolation, ~130-700ms overhead' },
  { value: 'bwrap', label: 'Bubblewrap (Linux)', desc: 'Lightweight Linux namespace isolation' },
]

export default function SandboxTab({ config, onChange }: Props) {
  const sandbox = config.sandbox ?? { enabled: false, mode: 'auto', allow_network: true }
  const docker = sandbox.docker ?? {}

  const updateSandbox = (updates: Partial<SandboxConfig>) => {
    onChange({ ...config, sandbox: { ...sandbox, ...updates } })
  }

  const updateDocker = (updates: Partial<DockerConfig>) => {
    updateSandbox({ docker: { ...docker, ...updates } })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Sandbox & Security</p>
        <p>
          Sandbox isolates tool commands (shell, file writes) to a per-user workspace directory.
          Each user gets their own workspace — files are invisible to other users.
        </p>
      </div>

      {/* Enable toggle */}
      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={() => updateSandbox({ enabled: !sandbox.enabled })}
          className={`relative w-10 h-5 rounded-full transition-colors ${sandbox.enabled ? 'bg-accent' : 'bg-gray-600'}`}
        >
          <span className={`absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full transition-transform ${sandbox.enabled ? 'translate-x-5' : ''}`} />
        </button>
        <label className="text-sm font-medium text-gray-300">Enable Sandbox</label>
      </div>

      {sandbox.enabled && (
        <>
          {/* Mode selector */}
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Sandbox Mode</label>
            <select
              value={sandbox.mode ?? 'auto'}
              onChange={(e) => updateSandbox({ mode: e.target.value as SandboxConfig['mode'] })}
              className="input max-w-[300px]"
            >
              {MODES.map((m) => (
                <option key={m.value} value={m.value}>{m.label}</option>
              ))}
            </select>
            <p className="text-[10px] text-gray-600 mt-1">
              {MODES.find((m) => m.value === (sandbox.mode ?? 'auto'))?.desc}
            </p>
          </div>

          {/* Network toggle */}
          <div className="flex items-center gap-3">
            <button
              type="button"
              onClick={() => updateSandbox({ allow_network: !sandbox.allow_network })}
              className={`relative w-10 h-5 rounded-full transition-colors ${sandbox.allow_network !== false ? 'bg-accent' : 'bg-gray-600'}`}
            >
              <span className={`absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full transition-transform ${sandbox.allow_network !== false ? 'translate-x-5' : ''}`} />
            </button>
            <div>
              <label className="text-sm font-medium text-gray-300">Allow Network</label>
              <p className="text-[10px] text-gray-600">
                {sandbox.allow_network !== false
                  ? 'Tool commands can access the internet'
                  : 'Tool commands run with no network (Docker: --network=none, macOS: deny network*)'}
              </p>
            </div>
          </div>

          {/* Docker settings (shown when mode is docker or auto) */}
          {(sandbox.mode === 'docker' || sandbox.mode === 'auto' || !sandbox.mode) && (
            <div className="space-y-3 p-3 bg-surface-dark/30 rounded-lg border border-gray-700/30">
              <p className="text-xs font-medium text-gray-400">Docker Settings</p>

              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Image</label>
                <input
                  type="text"
                  value={docker.image ?? ''}
                  onChange={(e) => updateDocker({ image: e.target.value || null })}
                  placeholder="ubuntu:24.04"
                  className="input max-w-[300px]"
                />
                <p className="text-[10px] text-gray-600 mt-1">
                  Docker image for sandbox containers. All users share the same image.
                </p>
              </div>

              <div className="grid grid-cols-3 gap-3">
                <div>
                  <label className="block text-sm font-medium text-gray-300 mb-1.5">CPU Limit</label>
                  <input
                    type="text"
                    value={docker.cpu_limit ?? ''}
                    onChange={(e) => updateDocker({ cpu_limit: e.target.value || null })}
                    placeholder="1.0"
                    className="input"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-300 mb-1.5">Memory Limit</label>
                  <input
                    type="text"
                    value={docker.memory_limit ?? ''}
                    onChange={(e) => updateDocker({ memory_limit: e.target.value || null })}
                    placeholder="512m"
                    className="input"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-300 mb-1.5">PID Limit</label>
                  <input
                    type="number"
                    value={docker.pids_limit ?? ''}
                    onChange={(e) => updateDocker({ pids_limit: e.target.value ? Number(e.target.value) : null })}
                    placeholder="256"
                    className="input"
                  />
                </div>
              </div>
            </div>
          )}

          {/* Comparison info */}
          <div className="text-xs text-gray-500 space-y-1 mt-2">
            <p className="font-medium text-gray-400">Backend Comparison</p>
            <table className="w-full text-left">
              <thead>
                <tr className="border-b border-gray-700/30">
                  <th className="py-1 pr-3">Feature</th>
                  <th className="py-1 pr-3">macOS sandbox-exec</th>
                  <th className="py-1">Docker</th>
                </tr>
              </thead>
              <tbody className="text-gray-500">
                <tr><td className="py-0.5 pr-3">Overhead</td><td className="pr-3">~6ms</td><td>~130-700ms</td></tr>
                <tr><td className="py-0.5 pr-3">Write isolation</td><td className="pr-3">SBPL subpath</td><td>Bind mount</td></tr>
                <tr><td className="py-0.5 pr-3">Read isolation</td><td className="pr-3">Host FS visible</td><td>Only workspace visible</td></tr>
                <tr><td className="py-0.5 pr-3">Network control</td><td className="pr-3">deny network*</td><td>--network=none</td></tr>
                <tr><td className="py-0.5 pr-3">Resource limits</td><td className="pr-3">No</td><td>CPU, memory, PIDs</td></tr>
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  )
}
