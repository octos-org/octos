import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  profileId?: string
}

export default function LineTab({ config, onChange, profileId }: Props) {
  const channel = config.channels.find((c) => c.type === 'line')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'line') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          {
            type: 'line',
            channel_secret_env: 'LINE_CHANNEL_SECRET',
            channel_access_token_env: 'LINE_CHANNEL_ACCESS_TOKEN',
          },
        ],
      })
    }
  }

  const updateField = (field: string, v: string | number | boolean | null) => {
    const channels = config.channels.map((c) => {
      if (c.type !== 'line') return c
      if (v === null) {
        const { [field]: _removed, ...rest } = c
        return { ...rest, type: c.type }
      }
      return { ...c, [field]: v }
    })
    onChange({ ...config, channels })
  }

  const updateEnv = (key: string, value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[key] = value
    } else {
      delete newEnvVars[key]
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  const updateAllowedSenders = (v: string) => {
    updateField('allowed_senders', v)
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">LINE Messaging API</p>
        <p>
          Connect your gateway to a LINE Official Account via webhooks. Supports text, images,
          audio, video, and file messages.
        </p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>
            Open the{' '}
            <a
              href="https://developers.line.biz/console/"
              target="_blank"
              rel="noopener"
              className="text-accent hover:underline"
            >
              LINE Developers Console
            </a>{' '}
            and create a Messaging API channel
          </li>
          <li>
            Under <strong>Basic settings</strong>, copy the <strong>Channel secret</strong>
          </li>
          <li>
            Under <strong>Messaging API</strong>, issue a <strong>Channel access token</strong>
          </li>
          <li>
            Enable <strong>Use webhook</strong> and paste the webhook URL below (use Verify to test)
          </li>
          <li>
            Subscribe to the <strong>message</strong> event (and optionally follow/unfollow)
          </li>
        </ol>
        <p className="text-gray-600">
          Requires a public HTTPS URL (or tunnel) pointing at this server. The gateway verifies
          requests with <code className="bg-gray-800 px-1 rounded">X-Line-Signature</code>.
        </p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable LINE channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Channel secret</label>
            <input
              type="password"
              value={config.env_vars['LINE_CHANNEL_SECRET'] || ''}
              onChange={(e) => updateEnv('LINE_CHANNEL_SECRET', e.target.value)}
              placeholder="Channel secret"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              From Basic settings. Stored as LINE_CHANNEL_SECRET.
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Channel access token
            </label>
            <input
              type="password"
              value={config.env_vars['LINE_CHANNEL_ACCESS_TOKEN'] || ''}
              onChange={(e) => updateEnv('LINE_CHANNEL_ACCESS_TOKEN', e.target.value)}
              placeholder="Long-lived channel access token"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              From Messaging API settings. Stored as LINE_CHANNEL_ACCESS_TOKEN.
            </p>
          </div>

          <label className="flex items-center gap-2 cursor-pointer">
            <input
              type="checkbox"
              checked={!!channel?.require_mention}
              onChange={(e) => updateField('require_mention', e.target.checked)}
              className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
            />
            <span className="text-sm text-gray-300">Require @mention in groups</span>
          </label>
          <p className="text-[10px] text-gray-600 -mt-2">
            When enabled, the bot only replies in LINE group and multi-person chats when
            @mentioned or sent a <code className="bg-gray-800 px-1 rounded">/command</code>.
            Direct messages are always answered.
          </p>

          {channel?.require_mention && (
            <div>
              <label className="block text-sm font-medium text-gray-300 mb-1.5">
                Bot user ID (optional)
              </label>
              <input
                value={channel?.bot_user_id || ''}
                onChange={(e) => updateField('bot_user_id', e.target.value)}
                placeholder="Uxxxxxxxx… (auto-detected if empty)"
                className="input text-xs font-mono"
              />
              <p className="text-[10px] text-gray-600 mt-1">
                LINE bot user ID for mention detection. Leave empty to fetch from the Messaging
                API on startup.
              </p>
            </div>
          )}

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Allowed senders
            </label>
            <input
              value={channel?.allowed_senders || ''}
              onChange={(e) => updateAllowedSenders(e.target.value)}
              placeholder="LINE user IDs, comma-separated (empty = allow all)"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Comma-separated LINE user IDs. Leave empty to allow any user who messages the bot.
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Webhook port</label>
            <input
              type="number"
              value={
                typeof channel?.webhook_port === 'number' ? channel.webhook_port : ''
              }
              onChange={(e) =>
                updateField(
                  'webhook_port',
                  e.target.value ? Number(e.target.value) : (null as any)
                )
              }
              placeholder="auto"
              className="input text-xs"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Leave blank for auto-assignment when the profile starts. Default is 8646.
            </p>
          </div>

          {profileId && (
            <div className="bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
              <label className="block text-sm font-medium text-gray-300 mb-1.5">Webhook URL</label>
              <div className="flex items-center gap-2">
                <code className="text-xs text-accent bg-gray-800 px-2 py-1 rounded flex-1 break-all select-all">
                  {window.location.origin}/webhook/line/{profileId}
                </code>
                <button
                  type="button"
                  onClick={() =>
                    navigator.clipboard.writeText(
                      `${window.location.origin}/webhook/line/${profileId}`
                    )
                  }
                  className="text-xs text-gray-400 hover:text-white px-2 py-1 rounded border border-gray-600 hover:border-gray-500"
                >
                  Copy
                </button>
              </div>
              <p className="text-[10px] text-gray-600 mt-1">
                Paste this URL into the LINE channel&apos;s Messaging API webhook settings. The
                server proxies events to this profile&apos;s gateway.
              </p>
            </div>
          )}
        </>
      )}
    </div>
  )
}
