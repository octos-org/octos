import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function QQBotTab({ config, onChange }: Props) {
  const channel = config.channels.find((c) => c.type === 'qq-bot')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'qq-bot') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          { type: 'qq-bot', app_id: '', client_secret_env: 'QQ_BOT_CLIENT_SECRET' },
        ],
      })
    }
  }

  const updateAppId = (v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'qq-bot' ? { ...c, app_id: v } : c
    )
    onChange({ ...config, channels })
  }

  const updateClientSecret = (v: string) => {
    const newEnvVars = { ...config.env_vars }
    if (v) {
      newEnvVars['QQ_BOT_CLIENT_SECRET'] = v
    } else {
      delete newEnvVars['QQ_BOT_CLIENT_SECRET']
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">QQ Bot (Official API v2)</p>
        <p>Connect your gateway to QQ as an official bot via WebSocket. The bot responds to @mentions in QQ groups and channels.</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Go to the <a href="https://open.qq.com/bot/" target="_blank" rel="noopener" className="text-accent hover:underline">QQ Open Platform</a> and create a bot application</li>
          <li>Copy the <strong>AppID</strong> from the bot details page</li>
          <li>Generate and copy the <strong>ClientSecret</strong> (save it immediately &mdash; it cannot be viewed again)</li>
          <li>Add the bot to your target QQ groups or channels</li>
        </ol>
        <p className="text-gray-600">Uses WebSocket gateway (no public IP required). Responds to @mentions in groups. Passive replies only (4 proactive messages/month limit).</p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable QQ Bot channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">App ID</label>
            <input
              type="text"
              value={(channel as any)?.app_id || ''}
              onChange={(e) => updateAppId(e.target.value)}
              placeholder="e.g. 102012345"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Found in QQ Open Platform &rarr; Bot Application &rarr; App Details.
            </p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Client Secret</label>
            <input
              type="password"
              value={config.env_vars['QQ_BOT_CLIENT_SECRET'] || ''}
              onChange={(e) => updateClientSecret(e.target.value)}
              placeholder="Client Secret from bot settings"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Generated in bot settings. Cannot be viewed again after creation. Stored as QQ_BOT_CLIENT_SECRET.
            </p>
          </div>
        </>
      )}
    </div>
  )
}
