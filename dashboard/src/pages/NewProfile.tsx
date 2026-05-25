import { useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { useToast } from '../components/Toast'
import { api } from '../api'
import type { ProfileConfig } from '../types'

export default function NewProfile() {
  const navigate = useNavigate()
  const [searchParams] = useSearchParams()
  const { toast } = useToast()
  const [loading, setLoading] = useState(false)
  const [id, setId] = useState('')
  const [name, setName] = useState('')
  const [publicSubdomain, setPublicSubdomain] = useState('')
  const [enabled, setEnabled] = useState(true)
  const [adminMode, setAdminMode] = useState(searchParams.get('adminMode') === 'true')

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      setLoading(true)
      const config: ProfileConfig | undefined = adminMode
        ? { channels: [], gateway: {}, env_vars: {}, admin_mode: true }
        : undefined
      await api.createProfile({
        id,
        name,
        public_subdomain: publicSubdomain.trim() || null,
        enabled,
        ...(config ? { config } : {}),
      })
      toast('Profile created')
      navigate(`/profile/${id}`)
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div>
      <div className="mb-6">
        <button
          onClick={() => navigate('/')}
          className="text-sm text-gray-500 hover:text-gray-300 mb-2 flex items-center gap-1"
        >
          <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 19l-7-7 7-7" />
          </svg>
          Back
        </button>
        <h1 className="text-2xl font-bold text-white">New Profile</h1>
        <p className="text-sm text-gray-500 mt-1">
          Create a new user profile. You can configure LLM providers, channels, and tools after creation.
        </p>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-6 max-w-lg">
        <form onSubmit={handleSubmit} className="space-y-4">
          <div>
            <label htmlFor="profile-id" className="block text-sm font-medium text-gray-300 mb-1.5">Profile ID</label>
            <p className="text-xs text-gray-500 mb-1.5">Lowercase letters, digits, hyphens. Cannot change after creation.</p>
            <input
              id="profile-id"
              value={id}
              onChange={(e) => setId(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
              placeholder="alice-bot"
              className="input"
              required
            />
          </div>
          <div>
            <label htmlFor="display-name" className="block text-sm font-medium text-gray-300 mb-1.5">Display Name</label>
            <input
              id="display-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Alice's Bot"
              className="input"
              required
            />
          </div>
          <div>
            <label htmlFor="public-subdomain" className="block text-sm font-medium text-gray-300 mb-1.5">Public Subdomain</label>
            <p className="text-xs text-gray-500 mb-1.5">Public URL slug. You can change this later without changing the internal profile ID.</p>
            <input
              id="public-subdomain"
              value={publicSubdomain}
              onChange={(e) => setPublicSubdomain(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
              placeholder={id || 'alice-bot'}
              className="input"
            />
          </div>
          <div>
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="checkbox"
                checked={enabled}
                onChange={(e) => setEnabled(e.target.checked)}
                className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
              />
              <span className="text-sm text-gray-400">Auto-start gateway when server starts</span>
            </label>
          </div>
          <div>
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="checkbox"
                checked={adminMode}
                onChange={(e) => setAdminMode(e.target.checked)}
                className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
              />
              <span className="text-sm text-gray-400">Admin mode (admin-only tools, no shell/file/web)</span>
            </label>
          </div>
          <div className="flex justify-end gap-3 pt-4 border-t border-gray-700/50">
            <button
              type="button"
              onClick={() => navigate('/')}
              className="px-4 py-2 text-sm font-medium text-gray-400 hover:text-white rounded-lg hover:bg-white/5 transition"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={loading || !id || !name}
              className="px-6 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
            >
              {loading ? 'Creating...' : 'Create Profile'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
