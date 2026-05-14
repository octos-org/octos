import { useEffect, useState } from 'react'
import { Navigate, useLocation } from 'react-router-dom'
import { ApiError, api } from '../api'
import { useAuth } from '../contexts/AuthContext'

// Result of the bootstrap check.
//   * `redirectTo: null` → render children (gate cleared, or we deliberately
//                          want to fall through to AuthGuard's login redirect
//                          on auth errors).
//   * `redirectTo: '/setup/welcome'`      → fresh admin who has never run the wizard.
//   * `redirectTo: '/setup/rotate-token'` → operator already finished the
//                                           welcome flow once; only the
//                                           rotate step is missing.
type GateDecision = { redirectTo: string | null; path: string }

const SETUP_PATHS = new Set(['/setup/welcome', '/setup/rotate-token'])

export default function BootstrapGate({ children }: { children: React.ReactNode }) {
  const { isAdmin } = useAuth()
  const location = useLocation()
  const [decision, setDecision] = useState<GateDecision | null>(null)

  useEffect(() => {
    if (!isAdmin) {
      // Non-admin users never see the rotation wizard.
      setDecision({ redirectTo: null, path: location.pathname })
      return
    }
    let cancelled = false

    // Probe both endpoints in parallel. If either auth check fails (401/403)
    // we bow out — AuthGuard upstream will redirect to /login. Other errors
    // (5xx, network) fall through to the conservative "not rotated" branch
    // so a misconfigured server still surfaces the wizard.
    Promise.allSettled([api.getTokenStatus(), api.getSetupState()]).then(
      ([statusRes, stateRes]) => {
        if (cancelled) return

        const authFailed =
          (statusRes.status === 'rejected' && ApiError.isAuthError(statusRes.reason)) ||
          (stateRes.status === 'rejected' && ApiError.isAuthError(stateRes.reason))
        if (authFailed) {
          // Let AuthGuard handle the redirect. Render children so the
          // unauthenticated path is invisible (no setup chrome flashes).
          setDecision({ redirectTo: null, path: location.pathname })
          return
        }

        const rotated =
          statusRes.status === 'fulfilled' ? statusRes.value.rotated : false
        const wizardCompleted =
          stateRes.status === 'fulfilled' && stateRes.value.wizard_completed_at != null

        if (rotated) {
          setDecision({ redirectTo: null, path: location.pathname })
          return
        }

        // Token not rotated. Skip the welcome step when the operator has
        // already completed the wizard once (re-rotation case).
        const target = wizardCompleted ? '/setup/rotate-token' : '/setup/welcome'
        setDecision({ redirectTo: target, path: location.pathname })
      },
    )

    return () => {
      cancelled = true
    }
  }, [isAdmin, location.pathname])

  // Loading: no verification yet, or stale for a previous path.
  if (decision === null || decision.path !== location.pathname) return null

  if (
    decision.redirectTo !== null &&
    !SETUP_PATHS.has(location.pathname)
  ) {
    return <Navigate to={decision.redirectTo} replace />
  }
  return <>{children}</>
}
