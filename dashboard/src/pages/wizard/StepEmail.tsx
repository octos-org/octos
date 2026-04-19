export default function StepEmail() {
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Email (SMTP)</h2>
        <p className="text-sm text-gray-400">
          octos uses SMTP for OTP login codes and outbound notifications.
        </p>
      </div>

      <div className="text-sm text-gray-300 bg-background/60 border border-gray-700/50 rounded-lg p-4 space-y-2">
        <p>
          SMTP settings aren't exposed in the dashboard yet. Configure them directly in
          <code className="mx-1 px-1 py-0.5 bg-white/5 rounded text-xs font-mono text-gray-200">config.json</code>
          under the <code className="mx-1 px-1 py-0.5 bg-white/5 rounded text-xs font-mono text-gray-200">smtp</code> key
          (host, port, username, password, from).
        </p>
        <p className="text-gray-400">
          If SMTP is not configured, OTP login emails fall back to a console log on the host.
        </p>
      </div>

      <p className="text-xs text-gray-500 border-t border-gray-700/50 pt-3">
        A future update will expose SMTP configuration directly from this wizard.
      </p>
    </div>
  )
}
