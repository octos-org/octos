import { createContext, useContext, useState, useEffect, useCallback, type ReactNode } from 'react'
import type { User } from '../types'
import { authApi } from '../api'

interface AuthContextValue {
  user: User | null
  token: string | null
  isAdmin: boolean
  loading: boolean
  sendOtp: (email: string) => Promise<{ ok: boolean; message?: string }>
  verifyOtp: (email: string, code: string) => Promise<boolean>
  loginWithToken: (token: string) => Promise<boolean>
  logout: () => Promise<void>
}

const AuthContext = createContext<AuthContextValue | null>(null)

export function AuthProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<User | null>(null)
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
      })
      .catch(() => {
        // Token invalid — clear it
        localStorage.removeItem('octos_session_token')
        setToken(null)
        setUser(null)
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
      return true
    } catch {
      localStorage.removeItem('octos_auth_token')
      return false
    }
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
  }, [])

  return (
    <AuthContext.Provider
      value={{ user, token, isAdmin, loading, sendOtp, verifyOtp, loginWithToken, logout }}
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
