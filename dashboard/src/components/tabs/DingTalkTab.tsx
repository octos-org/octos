import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  profileId?: string
}

export default function DingTalkTab({ config, onChange, profileId }: Props) {
  const channel = config.channels.find((c) => c.type === 'dingtalk')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'dingtalk') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          {
            type: 'dingtalk',
            webhook_url_env: 'DINGTALK_BOT_WEBHOOK',
            secret_env: 'DINGTALK_BOT_SECRET',
          },
        ],
      })
    }
  }

  const updateField = (field: string, value: string | number | null) => {
    const channels = config.channels.map((c) => {
      if (c.type !== 'dingtalk') return c
      if (value === null) {
        const { [field]: _removed, ...rest } = c
        return { ...rest, type: c.type }
      }
      return { ...c, [field]: value }
    })
    onChange({ ...config, channels })
  }

  const updateEnv = (key: string, value: string) => {
    const env_vars = { ...config.env_vars }
    if (value) {
      env_vars[key] = value
    } else {
      delete env_vars[key]
    }
    onChange({ ...config, env_vars })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">DingTalk Bot</p>
        <p>Connect a DingTalk custom robot for outbound messages and an outgoing robot webhook for inbound chat events.</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Create a DingTalk custom robot and copy its webhook URL</li>
          <li>Enable signature security and copy the robot secret</li>
          <li>For inbound messages, configure an outgoing robot callback URL</li>
        </ol>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable DingTalk channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Robot webhook URL
            </label>
            <input
              type="password"
              value={config.env_vars['DINGTALK_BOT_WEBHOOK'] || ''}
              onChange={(e) => updateEnv('DINGTALK_BOT_WEBHOOK', e.target.value)}
              placeholder="https://oapi.dingtalk.com/robot/send?access_token=..."
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Stored as DINGTALK_BOT_WEBHOOK. Used for proactive outbound sends.
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Robot secret
            </label>
            <input
              type="password"
              value={config.env_vars['DINGTALK_BOT_SECRET'] || ''}
              onChange={(e) => updateEnv('DINGTALK_BOT_SECRET', e.target.value)}
              placeholder="SEC..."
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Stored as DINGTALK_BOT_SECRET. Used for outbound signing and inbound verification.
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Allowed senders
            </label>
            <input
              value={channel?.allowed_senders || ''}
              onChange={(e) => updateField('allowed_senders', e.target.value)}
              placeholder="DingTalk staff IDs, comma-separated (empty = allow all)"
              className="input text-xs font-mono"
            />
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Webhook port</label>
            <input
              type="number"
              value={typeof channel?.webhook_port === 'number' ? channel.webhook_port : ''}
              onChange={(e) =>
                updateField('webhook_port', e.target.value ? Number(e.target.value) : null)
              }
              placeholder="auto"
              className="input text-xs"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Leave blank for auto-assignment when the profile starts. Default is 8650.
            </p>
          </div>

          {profileId && (
            <div className="bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
              <label className="block text-sm font-medium text-gray-300 mb-1.5">Callback URL</label>
              <div className="flex items-center gap-2">
                <code className="text-xs text-accent bg-gray-800 px-2 py-1 rounded flex-1 break-all select-all">
                  {window.location.origin}/webhook/dingtalk/{profileId}
                </code>
                <button
                  type="button"
                  onClick={() =>
                    navigator.clipboard.writeText(
                      `${window.location.origin}/webhook/dingtalk/${profileId}`
                    )
                  }
                  className="text-xs text-gray-400 hover:text-white px-2 py-1 rounded border border-gray-600 hover:border-gray-500"
                >
                  Copy
                </button>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  )
}
