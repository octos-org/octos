import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api } from '../../api'

export default function StepCreateProfile() {
  const navigate = useNavigate()
  const [working, setWorking] = useState<'create' | 'done' | null>(null)

  const finishWith = async (target: string, kind: 'create' | 'done') => {
    setWorking(kind)
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
        <h2 className="text-lg font-semibold text-white mb-1">All set</h2>
        <p className="text-sm text-gray-400">
          Your admin profile is configured (LLM, SMTP, deployment mode). You can start
          using the dashboard now. Want to create a separate profile — for someone else,
          or to keep a public-facing bot independent of admin? You can do that here, or
          any time later from the sidebar.
        </p>
      </div>

      <div className="bg-background/60 border border-gray-700/50 rounded-lg p-4 text-xs text-gray-400">
        <div className="text-gray-200 text-sm font-medium mb-1">When to create another profile</div>
        Profiles tie together an LLM, channels (Telegram / Discord / Slack / WhatsApp / Email / WeChat),
        tools, and skills. The admin profile is fine for personal use; create another one for a
        different user, a public-facing bot, or a profile with a unique subdomain.
      </div>

      <div className="space-y-2 pt-2">
        <button
          type="button"
          disabled={working !== null}
          onClick={() => finishWith('/', 'done')}
          className="w-full px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-50"
        >
          {working === 'done' ? 'Finishing…' : 'Done — Go to Dashboard'}
        </button>
        <button
          type="button"
          disabled={working !== null}
          onClick={() => finishWith('/profiles/new', 'create')}
          className="w-full px-4 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-50"
        >
          {working === 'create' ? 'Opening…' : 'Create another profile →'}
        </button>
      </div>
    </div>
  )
}
