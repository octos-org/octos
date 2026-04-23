export default function StepOverview() {
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">What's Next</h2>
        <p className="text-sm text-gray-400">
          A few quick steps and you'll have a working agent with login emails
          and the right deployment mode for your host.
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
          <div className="text-sm font-medium text-white mb-1">2. Configure email (SMTP)</div>
          <div className="text-xs text-gray-400">
            Used to deliver OTP login codes. Optional for local-only installs;
            required once you expose the dashboard on a tunnel.
          </div>
        </div>

        <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4">
          <div className="text-sm font-medium text-white mb-1">3. Pick a deployment mode</div>
          <div className="text-xs text-gray-400">
            <span className="font-mono text-gray-300">local</span> (default),{' '}
            <span className="font-mono text-gray-300">tenant</span> (frpc
            tunnel), or <span className="font-mono text-gray-300">cloud</span>{' '}
            (VPS relay). The wizard auto-detects and pre-selects the most
            likely choice.
          </div>
        </div>

        <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4">
          <div className="text-sm font-medium text-white mb-1">4. Create your first profile</div>
          <div className="text-xs text-gray-400">
            Profiles hold per-user config: the LLM to use, messaging channels
            (Telegram, Discord, Slack, Email, WhatsApp, WeChat), and
            tool/skill access. This is also where most day-to-day settings
            live.
          </div>
        </div>
      </div>
    </div>
  )
}
