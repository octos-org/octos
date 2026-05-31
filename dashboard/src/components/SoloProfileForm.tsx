import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'

// Client-side mirror of the server validators in `crate::api::ui_protocol`
// (`validate_local_name`, `normalize_local_username`, `validate_local_email`).
// These are a UX nicety only — the server remains authoritative and any
// rejection it returns is surfaced verbatim below the form.
const USERNAME_RE = /^[A-Za-z0-9._-]+$/
const EMAIL_RE = /^[^\s@]+@[^\s@]+$/

/**
 * The solo onboarding form: full name / username / email → `soloCreate`.
 * Shared by the login page (first run) and reusable elsewhere. On success
 * it calls `onDone` if provided, otherwise navigates to `/`.
 */
export default function SoloProfileForm({ onDone }: { onDone?: () => void }) {
  const { soloCreate } = useAuth()
  const navigate = useNavigate()
  const [name, setName] = useState('')
  const [username, setUsername] = useState('')
  const [email, setEmail] = useState('')
  const [error, setError] = useState('')
  const [submitting, setSubmitting] = useState(false)

  const nameOk = name.trim().length > 0 && name.trim().length <= 128
  const usernameOk =
    username.trim().length > 0 &&
    username.trim().length <= 64 &&
    USERNAME_RE.test(username.trim())
  const emailOk = EMAIL_RE.test(email.trim())
  const canSubmit = nameOk && usernameOk && emailOk && !submitting

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!canSubmit) return
    setError('')
    setSubmitting(true)
    try {
      await soloCreate({
        name: name.trim(),
        username: username.trim(),
        email: email.trim(),
      })
      if (onDone) onDone()
      else navigate('/', { replace: true })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not create profile')
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <form onSubmit={handleSubmit} data-testid="solo-profile-form">
      <p className="text-sm text-gray-400 mb-4">
        Create a local profile. It stays on this machine — no email code is sent.
      </p>

      <label className="block text-sm font-medium text-gray-300 mb-1" htmlFor="solo-name">
        Full name
      </label>
      <input
        id="solo-name"
        data-testid="solo-name"
        className="input w-full mb-3"
        value={name}
        onChange={(e) => setName(e.target.value)}
        autoFocus
        disabled={submitting}
      />

      <label className="block text-sm font-medium text-gray-300 mb-1" htmlFor="solo-username">
        Username
      </label>
      <input
        id="solo-username"
        data-testid="solo-username"
        className="input w-full mb-1 font-mono"
        value={username}
        onChange={(e) => setUsername(e.target.value)}
        disabled={submitting}
      />
      <p className="text-xs text-gray-600 mb-3">
        Letters, digits, dot, hyphen or underscore.
      </p>

      <label className="block text-sm font-medium text-gray-300 mb-1" htmlFor="solo-email">
        Email
      </label>
      <input
        id="solo-email"
        data-testid="solo-email"
        type="email"
        className="input w-full mb-4"
        value={email}
        onChange={(e) => setEmail(e.target.value)}
        disabled={submitting}
      />

      {error && (
        <p data-testid="solo-error" className="text-sm text-red-400 mb-3">
          {error}
        </p>
      )}

      <button
        type="submit"
        data-testid="solo-submit"
        disabled={!canSubmit}
        className="w-full px-4 py-2.5 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
      >
        {submitting ? 'Creating…' : 'Create profile & continue'}
      </button>
    </form>
  )
}
