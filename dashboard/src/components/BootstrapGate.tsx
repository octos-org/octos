import { useEffect, useState } from 'react'
import { Navigate, useLocation } from 'react-router-dom'
import { api } from '../api'
import { useAuth } from '../contexts/AuthContext'

type GateStatus = { rotated: boolean; path: string }

export default function BootstrapGate({ children }: { children: React.ReactNode }) {
  const { isAdmin } = useAuth()
  const location = useLocation()
  const [status, setStatus] = useState<GateStatus | null>(null)

  useEffect(() => {
    if (!isAdmin) {
      setStatus({ rotated: true, path: location.pathname })
      return
    }
    let cancelled = false
    api
      .getTokenStatus()
      .then((s) => {
        if (!cancelled) setStatus({ rotated: s.rotated, path: location.pathname })
      })
      .catch(() => {
        if (!cancelled) setStatus({ rotated: true, path: location.pathname })
      })
    return () => {
      cancelled = true
    }
  }, [isAdmin, location.pathname])

  // Loading: no verification yet, or stale for a previous path.
  if (status === null || status.path !== location.pathname) return null

  if (
    !status.rotated &&
    location.pathname !== '/setup/welcome' &&
    location.pathname !== '/setup/rotate-token'
  ) {
    return <Navigate to="/setup/welcome" replace />
  }
  return <>{children}</>
}
