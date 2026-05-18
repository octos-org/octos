import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api } from '../api'
import { useAuth } from '../contexts/AuthContext'

// Operator must type an admin password of *exactly* this length. Eight
// characters matches the server-side floor in `admin_setup.rs`
// (`MIN_ROTATED_TOKEN_LEN`); keeping the client at exactly-8 makes the
// constraint legible ("password is 8 characters") and avoids the
// auto-generated-32-char ceremony of previous releases.
const TOKEN_LEN = 8

export default function SetupRotateToken() {
  const { swapToken } = useAuth()
  const navigate = useNavigate()
  const [value, setValue] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [rotated, setRotated] = useState(false)
  const [reveal, setReveal] = useState(false)

  // LoginPage submits `adminToken.trim()`, so a token persisted with
  // leading/trailing whitespace would lock the operator out on next login.
  // Validate (and submit) the trimmed value so the two surfaces agree.
  const trimmed = value.trim()
  const hasSurroundingSpace = value.length !== trimmed.length
  const wrongLength = trimmed.length > 0 && trimmed.length !== TOKEN_LEN
  const valid = trimmed.length === TOKEN_LEN

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (rotated) {
      navigate('/setup/wizard', { replace: true })
      return
    }
    if (!valid) return
    setError('')
    setLoading(true)
    try {
      await api.rotateToken(trimmed)
      swapToken(trimmed)
      setRotated(true)
    } catch (e: any) {
      setError(e?.message || 'Failed to rotate token')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="min-h-screen flex items-center justify-center bg-background px-4">
      <div className="w-full max-w-lg bg-surface border border-gray-700/50 rounded-xl p-8 shadow-xl">
        <h1 className="text-xl font-bold text-white mb-2">Rotate Admin Token</h1>
        <p className="text-sm text-gray-400 mb-6">
          Replace the bootstrap token with a persistent admin password.
          Pick exactly {TOKEN_LEN} characters — you'll use this to sign back in.
        </p>

        <form onSubmit={handleSubmit} className="space-y-4">
          <div>
            <label
              htmlFor="admin-token-input"
              className="block text-xs font-medium text-gray-400 mb-1"
            >
              New admin token
            </label>
            <div className="flex gap-2">
              <input
                id="admin-token-input"
                type={reveal ? 'text' : 'password'}
                value={value}
                onChange={(e) => setValue(e.target.value)}
                readOnly={rotated}
                className="flex-1 px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent disabled:opacity-60 read-only:opacity-90"
                placeholder={`Exactly ${TOKEN_LEN} characters`}
                autoFocus
                autoComplete="new-password"
              />
              <button
                type="button"
                onClick={() => setReveal((r) => !r)}
                className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition"
              >
                {reveal ? 'Hide' : 'Show'}
              </button>
            </div>
            {wrongLength && (
              <p className="mt-1 text-xs text-yellow-400">
                Token must be exactly {TOKEN_LEN} characters ({trimmed.length}/{TOKEN_LEN}).
              </p>
            )}
            {hasSurroundingSpace && !wrongLength && (
              <p className="mt-1 text-xs text-yellow-400">
                Leading/trailing whitespace will be removed on submit (the login
                form also trims).
              </p>
            )}
          </div>

          {error && (
            <div className="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2">
              {error}
            </div>
          )}

          <button
            type="submit"
            disabled={loading || (!rotated && !valid)}
            className="w-full px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
          >
            {rotated
              ? "I've saved it, continue"
              : loading
              ? 'Submitting…'
              : 'Submit'}
          </button>
        </form>
      </div>
    </div>
  )
}
