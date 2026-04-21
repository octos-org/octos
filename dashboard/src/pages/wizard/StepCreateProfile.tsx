import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api } from '../../api'

export default function StepCreateProfile() {
  const navigate = useNavigate()
  const [working, setWorking] = useState<'create' | 'skip' | null>(null)

  const finishWith = async (target: string) => {
    setWorking(target === '/profiles/new' ? 'create' : 'skip')
    try {
      await api.completeSetup()
    } catch (e) {
      console.warn('completeSetup failed', e)
    }
    navigate(target)
  }

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Create your first profile</h2>
        <p className="text-sm text-gray-400">
          Your provider is set. A profile ties together the LLM, messaging channels, tools, and skills the assistant will use.
        </p>
      </div>

      <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4 text-xs text-gray-400">
        <div className="text-gray-200 text-sm font-medium mb-1">What happens on the next page</div>
        Pick a name, an LLM model, and (optionally) wire up a Telegram / Discord / Slack / WhatsApp / Email / WeChat channel. You can edit everything later from the sidebar.
      </div>

      <div className="space-y-2 pt-2">
        <button
          type="button"
          disabled={working !== null}
          onClick={() => finishWith('/profiles/new')}
          className="w-full px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-50"
        >
          {working === 'create' ? 'Opening…' : 'Create profile →'}
        </button>
        <button
          type="button"
          disabled={working !== null}
          onClick={() => finishWith('/')}
          className="w-full px-4 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-50"
        >
          {working === 'skip' ? 'Finishing…' : 'I\'ll do this later'}
        </button>
      </div>
    </div>
  )
}
