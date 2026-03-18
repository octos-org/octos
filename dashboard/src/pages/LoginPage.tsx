import { useState, useRef, useEffect } from 'react'
import { useNavigate } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'

export default function LoginPage() {
  const { user, sendOtp, verifyOtp, loginWithToken } = useAuth()
  const navigate = useNavigate()
  const [step, setStep] = useState<'email' | 'code' | 'token'>('email')
  const [email, setEmail] = useState('')
  const [code, setCode] = useState(['', '', '', '', '', ''])
  const [adminToken, setAdminToken] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [message, setMessage] = useState('')
  const codeRefs = useRef<(HTMLInputElement | null)[]>([])

  // Redirect if already logged in
  useEffect(() => {
    if (user) navigate('/', { replace: true })
  }, [user, navigate])

  const handleSendCode = async (e: React.FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      const res = await sendOtp(email)
      if (res.ok) {
        setMessage(res.message || 'Code sent')
        setStep('code')
        setTimeout(() => codeRefs.current[0]?.focus(), 100)
      } else {
        setError(res.message || 'Failed to send code')
      }
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }

  const handleCodeInput = (index: number, value: string) => {
    if (!/^\d*$/.test(value)) return
    const newCode = [...code]
    newCode[index] = value.slice(-1)
    setCode(newCode)

    // Auto-advance to next input
    if (value && index < 5) {
      codeRefs.current[index + 1]?.focus()
    }

    // Auto-submit when all 6 digits entered
    const fullCode = newCode.join('')
    if (fullCode.length === 6) {
      handleVerify(fullCode)
    }
  }

  const handleCodeKeyDown = (index: number, e: React.KeyboardEvent) => {
    if (e.key === 'Backspace' && !code[index] && index > 0) {
      codeRefs.current[index - 1]?.focus()
    }
  }

  const handleCodePaste = (e: React.ClipboardEvent) => {
    e.preventDefault()
    const pasted = e.clipboardData.getData('text').replace(/\D/g, '').slice(0, 6)
    if (pasted.length === 6) {
      const newCode = pasted.split('')
      setCode(newCode)
      handleVerify(pasted)
    }
  }

  const handleVerify = async (fullCode?: string) => {
    const codeStr = fullCode || code.join('')
    if (codeStr.length !== 6) return
    setError('')
    setLoading(true)
    try {
      const ok = await verifyOtp(email, codeStr)
      if (ok) {
        navigate('/', { replace: true })
      } else {
        setError('Invalid or expired code')
        setCode(['', '', '', '', '', ''])
        codeRefs.current[0]?.focus()
      }
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }

  const handleTokenLogin = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!adminToken.trim()) return
    setError('')
    setLoading(true)
    try {
      const ok = await loginWithToken(adminToken.trim())
      if (ok) {
        navigate('/', { replace: true })
      } else {
        setError('Invalid token')
      }
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="min-h-screen bg-background flex items-center justify-center px-4">
      <div className="w-full max-w-sm">
        {/* Logo */}
        <div className="text-center mb-8">
          <h1 className="text-2xl font-bold text-white">
            <span className="text-accent">octos</span>
          </h1>
          <p className="text-sm text-gray-500 mt-1">Sign in to your dashboard</p>
        </div>

        <div className="bg-surface rounded-xl border border-gray-700/50 p-6">
          {step === 'token' ? (
            <form onSubmit={handleTokenLogin}>
              <label className="block text-sm font-medium text-gray-300 mb-2">
                Admin token
              </label>
              <input
                type="password"
                value={adminToken}
                onChange={(e) => setAdminToken(e.target.value)}
                placeholder="Enter --auth-token value"
                className="input w-full mb-4 font-mono"
                required
                autoFocus
                disabled={loading}
              />
              {error && (
                <p className="text-sm text-red-400 mb-3">{error}</p>
              )}
              <button
                type="submit"
                disabled={loading || !adminToken.trim()}
                className="w-full px-4 py-2.5 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50 mb-3"
              >
                {loading ? 'Verifying...' : 'Login'}
              </button>
              <button
                type="button"
                onClick={() => { setStep('email'); setError(''); setAdminToken('') }}
                className="w-full text-sm text-gray-500 hover:text-gray-300 transition"
              >
                Login with email instead
              </button>
            </form>
          ) : step === 'email' ? (
            <form onSubmit={handleSendCode}>
              <label className="block text-sm font-medium text-gray-300 mb-2">
                Email address
              </label>
              <input
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="you@example.com"
                className="input w-full mb-4"
                required
                autoFocus
                disabled={loading}
              />
              {error && (
                <p className="text-sm text-red-400 mb-3">{error}</p>
              )}
              <button
                type="submit"
                disabled={loading || !email}
                className="w-full px-4 py-2.5 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
              >
                {loading ? 'Sending...' : 'Send verification code'}
              </button>
            </form>
          ) : (
            <div>
              <p className="text-sm text-gray-400 mb-1">
                Enter the 6-digit code sent to
              </p>
              <p className="text-sm text-white font-medium mb-4">{email}</p>

              {message && (
                <p className="text-xs text-green-400 mb-3">{message}</p>
              )}

              <div className="flex gap-2 justify-center mb-4" onPaste={handleCodePaste}>
                {code.map((digit, i) => (
                  <input
                    key={i}
                    ref={(el) => { codeRefs.current[i] = el }}
                    type="text"
                    inputMode="numeric"
                    maxLength={1}
                    value={digit}
                    onChange={(e) => handleCodeInput(i, e.target.value)}
                    onKeyDown={(e) => handleCodeKeyDown(i, e)}
                    className="w-11 h-12 text-center text-lg font-mono rounded-lg bg-surface-dark border border-gray-600 text-white focus:border-accent focus:outline-none"
                    disabled={loading}
                  />
                ))}
              </div>

              {error && (
                <p className="text-sm text-red-400 mb-3 text-center">{error}</p>
              )}

              <button
                onClick={() => handleVerify()}
                disabled={loading || code.join('').length !== 6}
                className="w-full px-4 py-2.5 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50 mb-3"
              >
                {loading ? 'Verifying...' : 'Verify'}
              </button>

              <button
                type="button"
                onClick={() => {
                  setStep('email')
                  setCode(['', '', '', '', '', ''])
                  setError('')
                  setMessage('')
                }}
                className="w-full text-sm text-gray-500 hover:text-gray-300 transition"
              >
                Use a different email
              </button>
            </div>
          )}
        </div>

        {/* Toggle between email and token login */}
        {step !== 'code' && step !== 'token' && (
          <div className="text-center mt-4">
            <button
              type="button"
              onClick={() => { setStep('token'); setError('') }}
              className="text-xs text-gray-600 hover:text-gray-400 transition"
            >
              Login with admin token
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
