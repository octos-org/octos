import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api } from '../api'
import { useAuth } from '../contexts/AuthContext'

const BASE62 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'

function generateToken(): string {
  const arr = new Uint8Array(32)
  crypto.getRandomValues(arr)
  return Array.from(arr, (b) => BASE62[b % BASE62.length]).join('')
}

export default function SetupRotateToken() {
  const { swapToken } = useAuth()
  const navigate = useNavigate()
  const [value, setValue] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [rotated, setRotated] = useState(false)
  const [copied, setCopied] = useState(false)

  const handleGenerate = () => {
    setValue(generateToken())
    setCopied(false)
    setError('')
  }

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch (e: any) {
      setError(e.message || 'Failed to copy')
    }
  }

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      await api.rotateToken(value)
      swapToken(value)
      setRotated(true)
    } catch (e: any) {
      setError(e.message || 'Failed to rotate token')
    } finally {
      setLoading(false)
    }
  }

  const handleContinue = () => {
    navigate('/setup/wizard', { replace: true })
  }

  return (
    <div className="min-h-screen flex items-center justify-center bg-background px-4">
      <div className="w-full max-w-lg bg-surface border border-gray-700/50 rounded-xl p-8 shadow-xl">
        <h1 className="text-xl font-bold text-white mb-2">Rotate Admin Token</h1>
        <p className="text-sm text-gray-400 mb-6">
          Replace the bootstrap token with a persistent hashed admin token. This is
          required before you can access the dashboard.
        </p>

        {!rotated && (
          <form onSubmit={handleSubmit} className="space-y-4">
            <div>
              <label className="block text-xs font-medium text-gray-400 mb-1">
                New admin token
              </label>
              <div className="flex gap-2">
                <input
                  type="text"
                  value={value}
                  onChange={(e) => setValue(e.target.value)}
                  className="flex-1 px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
                  placeholder="At least 32 characters"
                  autoFocus
                />
                <button
                  type="button"
                  onClick={handleGenerate}
                  className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition"
                >
                  Generate
                </button>
                <button
                  type="button"
                  onClick={handleCopy}
                  disabled={!value}
                  className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
                >
                  {copied ? 'Copied' : 'Copy'}
                </button>
              </div>
            </div>

            {error && (
              <div className="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-lg px-3 py-2">
                {error}
              </div>
            )}

            <button
              type="submit"
              disabled={loading || !value}
              className="w-full px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {loading ? 'Submitting…' : 'Submit'}
            </button>
          </form>
        )}

        {rotated && (
          <div className="space-y-4">
            <div className="text-sm text-yellow-300 bg-yellow-500/10 border border-yellow-500/30 rounded-lg px-3 py-3">
              Save this somewhere safe — it won't be shown again.
            </div>
            <div className="px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono break-all">
              {value}
            </div>
            <button
              type="button"
              onClick={handleCopy}
              className="w-full px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition"
            >
              {copied ? 'Copied' : 'Copy token'}
            </button>
            <button
              type="button"
              onClick={handleContinue}
              className="w-full px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition"
            >
              I've saved it, continue
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
