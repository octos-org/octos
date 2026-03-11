import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function WeComBotTab({ config, onChange }: Props) {
  const channel = config.channels.find((c) => c.type === 'wecom-bot')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'wecom-bot') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          { type: 'wecom-bot', bot_id: '', secret_env: 'WECOM_BOT_SECRET' },
        ],
      })
    }
  }

  const updateBotId = (v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'wecom-bot' ? { ...c, bot_id: v } : c
    )
    onChange({ ...config, channels })
  }

  const updateSecret = (v: string) => {
    const newEnvVars = { ...config.env_vars }
    if (v) {
      newEnvVars['WECOM_BOT_SECRET'] = v
    } else {
      delete newEnvVars['WECOM_BOT_SECRET']
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">WeCom Group Robot</p>
        <p>Connect your gateway to a WeCom (Enterprise WeChat) group robot via WebSocket long connection. The bot can receive messages when @mentioned in group chats.</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Open the <a href="https://work.weixin.qq.com/wework_admin/frame#/app/servicer" target="_blank" rel="noopener" className="text-accent hover:underline">WeCom Admin Console</a> and go to <strong>Application Management</strong></li>
          <li>Create a <strong>Group Robot</strong> (群机器人) or find your existing one</li>
          <li>Copy the <strong>Bot ID</strong> from the robot details page</li>
          <li>Under <strong>Development Configuration</strong>, generate or copy the <strong>Secret</strong></li>
          <li>Add the robot to your target group chats</li>
        </ol>
        <p className="text-gray-600">Uses WebSocket long connection (no public IP required). Responds to @mentions in group chats.</p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable WeCom Bot channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Bot ID</label>
            <input
              type="text"
              value={(channel as any)?.bot_id || ''}
              onChange={(e) => updateBotId(e.target.value)}
              placeholder="e.g. wkxxxxxx"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Found in WeCom Admin &rarr; Application Management &rarr; Group Robot details.
            </p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Secret</label>
            <input
              type="password"
              value={config.env_vars['WECOM_BOT_SECRET'] || ''}
              onChange={(e) => updateSecret(e.target.value)}
              placeholder="Secret from Development Configuration"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              From Development Configuration in the robot settings. Stored as WECOM_BOT_SECRET.
            </p>
          </div>
        </>
      )}
    </div>
  )
}
