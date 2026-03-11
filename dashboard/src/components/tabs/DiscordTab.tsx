import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function DiscordTab({ config, onChange }: Props) {
  const channel = config.channels.find((c) => c.type === 'discord')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'discord') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          { type: 'discord', token_env: 'DISCORD_BOT_TOKEN', allowed_senders: '' },
        ],
      })
    }
  }

  const updateBotToken = (v: string) => {
    const newEnvVars = { ...config.env_vars }
    if (v) {
      newEnvVars['DISCORD_BOT_TOKEN'] = v
    } else {
      delete newEnvVars['DISCORD_BOT_TOKEN']
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  const updateAllowedSenders = (v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'discord' ? { ...c, allowed_senders: v } : c
    )
    onChange({ ...config, channels })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Discord Bot</p>
        <p>Connect your gateway to Discord as a bot. Supports text messages, attachments, and DMs.</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Go to the <a href="https://discord.com/developers/applications" target="_blank" rel="noopener" className="text-accent hover:underline">Discord Developer Portal</a> and create an application</li>
          <li>Under <strong>Bot</strong>, click <em>Reset Token</em> and copy the bot token</li>
          <li>Enable <strong>Message Content Intent</strong> under Privileged Gateway Intents</li>
          <li>Under <strong>OAuth2 &rarr; URL Generator</strong>, select <code className="bg-gray-800 px-1 rounded">bot</code> scope with <code className="bg-gray-800 px-1 rounded">Send Messages</code> + <code className="bg-gray-800 px-1 rounded">Read Message History</code> permissions, then invite the bot to your server</li>
        </ol>
        <p className="text-gray-600">Uses Discord Gateway (WebSocket). Responds to DMs and mentions in guilds.</p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable Discord channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Bot Token</label>
            <input
              type="password"
              value={config.env_vars['DISCORD_BOT_TOKEN'] || ''}
              onChange={(e) => updateBotToken(e.target.value)}
              placeholder="MTIzNDU2Nzg5MDEy..."
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Get this from the Discord Developer Portal &rarr; Bot &rarr; Token. Stored as DISCORD_BOT_TOKEN.
            </p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Allowed Senders
            </label>
            <input
              value={channel?.allowed_senders || ''}
              onChange={(e) => updateAllowedSenders(e.target.value)}
              placeholder="Discord user IDs, comma-separated (empty = allow all)"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Comma-separated Discord user IDs (numeric). Leave empty to allow anyone. Enable Developer Mode in Discord settings to copy IDs.
            </p>
          </div>
        </>
      )}
    </div>
  )
}
