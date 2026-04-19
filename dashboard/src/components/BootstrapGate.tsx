import { useEffect, useState } from 'react'
import { Navigate, useLocation } from 'react-router-dom'
import { api } from '../api'
import { useAuth } from '../contexts/AuthContext'

export default function BootstrapGate({ children }: { children: React.ReactNode }) {
  const { isAdmin } = useAuth()
  const location = useLocation()
  const [rotated, setRotated] = useState<boolean | null>(null)

  useEffect(() => {
    if (!isAdmin) {
      setRotated(true)
      return
    }
    api
      .getTokenStatus()
      .then((s) => setRotated(s.rotated))
      .catch(() => setRotated(true))
  }, [isAdmin])

  if (rotated === null) return null
  if (!rotated && location.pathname !== '/setup/rotate-token') {
    return <Navigate to="/setup/rotate-token" replace />
  }
  return <>{children}</>
}
