import { Link } from 'react-router-dom'

export default function StepChannel() {
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Messaging channel</h2>
        <p className="text-sm text-gray-400">
          Connect octos to Telegram, Discord, Slack, WhatsApp, Email, or WeChat.
        </p>
      </div>

      <div className="text-sm text-gray-300 bg-background/60 border border-gray-700/50 rounded-lg p-4 space-y-2">
        <p>
          Messaging channels are configured <strong className="text-white">per profile</strong>, not globally.
          The fastest way to set one up is to create a profile and pick the channel type
          during creation.
        </p>
      </div>

      <Link
        to="/profiles/new"
        className="inline-block px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition"
      >
        Create a profile →
      </Link>
    </div>
  )
}
