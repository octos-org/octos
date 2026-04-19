export default function StepDeployMode() {
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Deployment mode</h2>
        <p className="text-sm text-gray-400">
          How this octos host is reachable from the outside world.
        </p>
      </div>

      <ul className="text-sm text-gray-300 space-y-2">
        <li className="bg-background/60 border border-gray-700/50 rounded-lg p-3">
          <span className="font-medium text-white">local</span>
          <span className="text-gray-400"> — dashboard and API stay on this machine. No public URL.</span>
        </li>
        <li className="bg-background/60 border border-gray-700/50 rounded-lg p-3">
          <span className="font-medium text-white">tenant</span>
          <span className="text-gray-400"> — this host dials out through an <code className="mx-1 px-1 py-0.5 bg-white/5 rounded text-xs font-mono text-gray-200">frpc</code> tunnel to a shared relay.</span>
        </li>
        <li className="bg-background/60 border border-gray-700/50 rounded-lg p-3">
          <span className="font-medium text-white">cloud</span>
          <span className="text-gray-400"> — this host <em>is</em> the relay VPS, accepting tunnels from other tenants.</span>
        </li>
      </ul>

      <p className="text-xs text-gray-500 border-t border-gray-700/50 pt-3">
        The mode is set via <code className="mx-1 px-1 py-0.5 bg-white/5 rounded text-xs font-mono text-gray-200">config.json</code> →
        <code className="mx-1 px-1 py-0.5 bg-white/5 rounded text-xs font-mono text-gray-200">"mode"</code>.
        Dashboard-driven switching is not available yet.
      </p>
    </div>
  )
}
