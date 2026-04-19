import { Link } from 'react-router-dom'

export default function SetupWelcome() {
  return (
    <div className="min-h-screen flex items-center justify-center bg-background px-4">
      <div className="w-full max-w-lg bg-surface border border-gray-700/50 rounded-xl p-8 shadow-xl">
        <h1 className="text-2xl font-bold text-white mb-2">Welcome to octos</h1>
        <p className="text-sm text-gray-400 mb-6">
          Two quick steps before you're fully set up.
        </p>

        <ol className="space-y-4 mb-8">
          <li className="flex gap-3">
            <span className="flex-shrink-0 w-6 h-6 rounded-full bg-accent/20 text-accent text-xs font-semibold flex items-center justify-center mt-0.5">
              1
            </span>
            <div>
              <div className="text-sm font-medium text-white">Create an admin token</div>
              <div className="text-sm text-gray-400">
                Replaces the default bootstrap token and becomes your dashboard login for this host.
              </div>
            </div>
          </li>
          <li className="flex gap-3">
            <span className="flex-shrink-0 w-6 h-6 rounded-full bg-accent/20 text-accent text-xs font-semibold flex items-center justify-center mt-0.5">
              2
            </span>
            <div>
              <div className="text-sm font-medium text-white">
                Setup wizard <span className="text-gray-500 font-normal">(optional)</span>
              </div>
              <div className="text-sm text-gray-400">
                Guides you through LLM providers, email (SMTP), a messaging channel, and deployment mode. Skip any step and revisit from the sidebar anytime.
              </div>
            </div>
          </li>
        </ol>

        <Link
          to="/setup/rotate-token"
          className="block w-full text-center px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition"
        >
          Get started
        </Link>
      </div>
    </div>
  )
}
