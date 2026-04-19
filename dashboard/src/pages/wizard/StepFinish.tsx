import { useEffect, useState } from 'react'
import { api, type SetupState } from '../../api'

export default function StepFinish() {
  const [state, setState] = useState<SetupState | null>(null)
  const [error, setError] = useState('')

  useEffect(() => {
    let cancelled = false
    api
      .getSetupState()
      .then((s) => {
        if (!cancelled) setState(s)
      })
      .catch((e) => {
        if (!cancelled) setError(e?.message || 'Failed to load setup state')
      })
    return () => {
      cancelled = true
    }
  }, [])

  const lastStep = state?.wizard_last_step_reached ?? 0

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Setup complete</h2>
        <p className="text-sm text-gray-400">
          You're all set. You can revisit this wizard anytime from the sidebar.
        </p>
      </div>

      <div className="text-sm text-gray-300 bg-background/60 border border-gray-700/50 rounded-lg p-4">
        {error ? (
          <span className="text-red-400">{error}</span>
        ) : state ? (
          <>
            <div className="text-gray-400">
              Last step reached: <span className="text-white font-mono">{lastStep + 1}</span>
            </div>
            {state.wizard_completed_at && (
              <div className="text-gray-400 mt-1">
                Previously completed: <span className="text-white">{new Date(state.wizard_completed_at).toLocaleString()}</span>
              </div>
            )}
            {state.wizard_skipped && !state.wizard_completed_at && (
              <div className="text-gray-400 mt-1">Previously skipped.</div>
            )}
          </>
        ) : (
          <span className="text-gray-500">Loading…</span>
        )}
      </div>

      <p className="text-xs text-gray-500 border-t border-gray-700/50 pt-3">
        Press <span className="text-gray-300 font-medium">Finish</span> below to mark setup complete and go to the dashboard.
      </p>
    </div>
  )
}
