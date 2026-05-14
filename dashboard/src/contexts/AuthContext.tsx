import { createContext, useContext, useState, useEffect, useCallback, type ReactNode } from 'react'
import type { ScopedAuthTarget, User } from '../types'
import { authApi } from '../api'

interface AuthContextValue {
  user: User | null
  token: string | null
  isAdmin: boolean
  /** Tenant scope derived from the request host (server's
   *  `host_scoped_profile_id`). `null` on the root domain / direct IP
   *  / localhost; populated when the dashboard is loaded from a
   *  tenant subdomain (e.g. `dspfac.ocean.ominix.io`). Used by the
   *  Sidebar to hide admin-global navigation while operating inside a
   *  tenant scope — Option Y, issue #315. */
  scopedProfile: ScopedAuthTarget | null
  loading: boolean
  sendOtp: (email: string) => Promise<{ ok: boolean; message?: string }>
  verifyOtp: (email: string, code: string) => Promise<boolean>
  loginWithToken: (token: string) => Promise<boolean>
  swapToken: (newToken: string) => void
  logout: () => Promise<void>
}

const AuthContext = createContext<AuthContextValue | null>(null)

export function AuthProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<User | null>(null)
  const [scopedProfile, setScopedProfile] = useState<ScopedAuthTarget | null>(null)
  const [token, setToken] = useState<string | null>(
    () => localStorage.getItem('octos_session_token') || localStorage.getItem('octos_auth_token')
  )
  const [loading, setLoading] = useState(true)

  const isAdmin = user?.role === 'admin'

  // On mount, validate stored session
  useEffect(() => {
    if (!token) {
      setLoading(false)
      return
    }

    authApi.me()
      .then((res) => {
        setUser(res.user)
        setScopedProfile(res.scoped_profile ?? null)
      })
      .catch(() => {
        // Token invalid — clear it
        localStorage.removeItem('octos_session_token')
        setToken(null)
        setUser(null)
        setScopedProfile(null)
      })
      .finally(() => setLoading(false))
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  const sendOtp = useCallback(async (email: string) => {
    const res = await authApi.sendCode(email)
    return { ok: res.ok, message: res.message }
  }, [])

  const verifyOtp = useCallback(async (email: string, code: string) => {
    const res = await authApi.verify(email, code)
    if (res.ok && res.token) {
      localStorage.setItem('octos_session_token', res.token)
      setToken(res.token)
      if (res.user) setUser(res.user)
      // Refresh the host scope after a successful verify — the OTP
      // flow doesn't return it, but `/api/auth/me` does.
      try {
        const me = await authApi.me()
        setScopedProfile(me.scoped_profile ?? null)
      } catch {
        // best-effort
      }
      return true
    }
    return false
  }, [])

  const loginWithToken = useCallback(async (adminToken: string) => {
    // Store the token and try /api/auth/me to validate it
    localStorage.setItem('octos_auth_token', adminToken)
    try {
      const res = await authApi.me()
      setToken(adminToken)
      setUser(res.user)
      setScopedProfile(res.scoped_profile ?? null)
      return true
    } catch {
      localStorage.removeItem('octos_auth_token')
      return false
    }
  }, [])

  const swapToken = useCallback((newToken: string) => {
    localStorage.setItem('octos_auth_token', newToken)
    setToken(newToken)
  }, [])

  const logout = useCallback(async () => {
    try {
      await authApi.logout()
    } catch {
      // ignore
    }
    localStorage.removeItem('octos_session_token')
    localStorage.removeItem('octos_auth_token')
    setToken(null)
    setUser(null)
    setScopedProfile(null)
  }, [])

  return (
    <AuthContext.Provider
      value={{ user, token, isAdmin, scopedProfile, loading, sendOtp, verifyOtp, loginWithToken, swapToken, logout }}
    >
      {children}
    </AuthContext.Provider>
  )
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext)
  if (!ctx) throw new Error('useAuth must be used within AuthProvider')
  return ctx
}
