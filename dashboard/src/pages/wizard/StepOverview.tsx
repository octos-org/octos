export default function StepOverview() {
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">What's next</h2>
        <p className="text-sm text-gray-400">
          A couple of quick steps, plus a few settings worth knowing about that live outside the wizard.
        </p>
      </div>

      <div className="space-y-3">
        <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4">
          <div className="text-sm font-medium text-white mb-1">1. Test an LLM provider</div>
          <div className="text-xs text-gray-400">
            Pick a provider and verify your API key works before using it.
          </div>
        </div>

        <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4">
          <div className="text-sm font-medium text-white mb-1">2. Create your first profile</div>
          <div className="text-xs text-gray-400">
            Profiles hold per-user config: the LLM to use, messaging channels (Telegram, Discord, Slack, Email, WhatsApp, WeChat), and tool/skill access. This is also where most day-to-day settings live.
          </div>
        </div>
      </div>

      <div className="bg-surface-dark/60 border border-gray-700/50 rounded-lg p-4">
        <div className="text-xs font-semibold text-gray-300 mb-2 uppercase tracking-wide">
          Configured outside the wizard
        </div>
        <ul className="space-y-2 text-xs text-gray-400">
          <li>
            <span className="text-gray-200">Email (SMTP)</span> — used for OTP login emails and notifications. Lives under <span className="font-mono text-gray-300">smtp</span> in <span className="font-mono text-gray-300">config.json</span>. Not required for basic local use.
          </li>
          <li>
            <span className="text-gray-200">Deployment mode</span> — <span className="font-mono text-gray-300">local</span> (default), <span className="font-mono text-gray-300">tenant</span> (frpc tunnel), or <span className="font-mono text-gray-300">cloud</span> (VPS relay). Set via <span className="font-mono text-gray-300">mode</span> in <span className="font-mono text-gray-300">config.json</span>.
          </li>
        </ul>
      </div>
    </div>
  )
}
