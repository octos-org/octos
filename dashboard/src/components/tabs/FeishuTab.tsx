import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  profileId?: string
}

export default function FeishuTab({ config, onChange, profileId }: Props) {
  const channel = config.channels.find((c) => c.type === 'feishu')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'feishu') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          {
            type: 'feishu',
            app_id_env: 'FEISHU_APP_ID',
            app_secret_env: 'FEISHU_APP_SECRET',
          },
        ],
      })
    }
  }

  const updateField = (field: string, v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'feishu' ? { ...c, [field]: v } : c
    )
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

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Feishu / Lark</p>
        <p>Connect to Feishu (China) or Lark (international) by ByteDance. Supports text, rich text, images, files, and card messages.</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Go to <a href="https://open.feishu.cn/" target="_blank" rel="noopener" className="text-accent hover:underline">Feishu Open Platform</a> or <a href="https://open.larksuite.com/" target="_blank" rel="noopener" className="text-accent hover:underline">Lark Developer</a> and create a custom app</li>
          <li>Enable <strong>Bot</strong> capability under App Features</li>
          <li>Copy <strong>App ID</strong> and <strong>App Secret</strong> from the credentials page</li>
          <li>Under Event Subscriptions, select <strong>persistent connection</strong> mode and subscribe to <code className="bg-gray-800 px-1 rounded">im.message.receive_v1</code> (Message received)</li>
          <li>Add permissions: <code className="bg-gray-800 px-1 rounded">im:message</code>, <code className="bg-gray-800 px-1 rounded">im:message:send_as_bot</code>, <code className="bg-gray-800 px-1 rounded">im:message.p2p_msg:readonly</code></li>
          <li>Publish a version of the app (Create Version)</li>
        </ol>
        <p className="text-gray-600"><strong>WebSocket mode</strong> (recommended): No public URL needed, the bot connects outbound. <strong>Webhook mode</strong>: Requires Verification Token, Encrypt Key, and a public URL.</p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable Feishu / Lark channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">App ID</label>
            <input
              type="password"
              value={config.env_vars['FEISHU_APP_ID'] || ''}
              onChange={(e) => updateEnv('FEISHU_APP_ID', e.target.value)}
              placeholder="cli_xxxx"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">From app credentials page. Stored as FEISHU_APP_ID.</p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">App Secret</label>
            <input
              type="password"
              value={config.env_vars['FEISHU_APP_SECRET'] || ''}
              onChange={(e) => updateEnv('FEISHU_APP_SECRET', e.target.value)}
              placeholder="secret..."
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">From app credentials page. Stored as FEISHU_APP_SECRET.</p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Mode</label>
            <select
              value={(channel as any)?.mode || 'websocket'}
              onChange={(e) => updateField('mode', e.target.value)}
              className="input text-xs"
            >
              <option value="websocket">WebSocket (recommended, no public URL needed)</option>
              <option value="webhook">Webhook (requires public URL / ngrok)</option>
            </select>
            <p className="text-[10px] text-gray-600 mt-1">
              WebSocket connects outbound (no port conflicts). Webhook requires a unique port per profile and a public URL.
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Region</label>
            <select
              value={(channel as any)?.region || 'feishu'}
              onChange={(e) => updateField('region', e.target.value)}
              className="input text-xs"
            >
              <option value="feishu">Feishu (China)</option>
              <option value="lark">Lark (International)</option>
            </select>
            <p className="text-[10px] text-gray-600 mt-1">
              Determines API endpoint: feishu.cn or larksuite.com
            </p>
          </div>

          {((channel as any)?.mode || 'websocket') === 'websocket' && (
            <div className="bg-green-500/10 border border-green-500/20 rounded-lg p-3 text-xs text-green-400">
              WebSocket mode: The gateway connects outbound to Feishu servers. No public URL or port configuration needed. Each profile connects independently.
            </div>
          )}

          {(channel as any)?.mode === 'webhook' && (
            <>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Verification Token</label>
                <input
                  type="password"
                  value={config.env_vars['FEISHU_VERIFICATION_TOKEN'] || ''}
                  onChange={(e) => updateEnv('FEISHU_VERIFICATION_TOKEN', e.target.value)}
                  placeholder="verification token (optional)"
                  className="input text-xs font-mono"
                />
                <p className="text-[10px] text-gray-600 mt-1">Optional. From Event Subscriptions settings for signature validation.</p>
              </div>

              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Encrypt Key</label>
                <input
                  type="password"
                  value={config.env_vars['FEISHU_ENCRYPT_KEY'] || ''}
                  onChange={(e) => updateEnv('FEISHU_ENCRYPT_KEY', e.target.value)}
                  placeholder="encrypt key (optional)"
                  className="input text-xs font-mono"
                />
                <p className="text-[10px] text-gray-600 mt-1">Optional. From Event Subscriptions settings for AES-256-CBC event decryption.</p>
              </div>

              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Webhook Port</label>
                <input
                  type="number"
                  value={(channel as any)?.webhook_port || ''}
                  onChange={(e) => updateField('webhook_port', e.target.value ? Number(e.target.value) as any : null as any)}
                  placeholder="auto"
                  className="input text-xs"
                />
                <p className="text-[10px] text-gray-600 mt-1">
                  Leave blank for auto-assignment by the server.
                </p>
              </div>

              {profileId && (
                <div className="bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
                  <label className="block text-sm font-medium text-gray-300 mb-1.5">Webhook URL</label>
                  <div className="flex items-center gap-2">
                    <code className="text-xs text-accent bg-gray-800 px-2 py-1 rounded flex-1 break-all select-all">
                      {window.location.origin}/webhook/feishu/{profileId}
                    </code>
                    <button
                      type="button"
                      onClick={() => navigator.clipboard.writeText(`${window.location.origin}/webhook/feishu/${profileId}`)}
                      className="text-xs text-gray-400 hover:text-white px-2 py-1 rounded border border-gray-600 hover:border-gray-500"
                    >
                      Copy
                    </button>
                  </div>
                  <p className="text-[10px] text-gray-600 mt-1">
                    Paste this URL into the Feishu/Lark app's Event Subscription settings. The server will proxy events to this profile's gateway.
                  </p>
                </div>
              )}
            </>
          )}
        </>
      )}
    </div>
  )
}
