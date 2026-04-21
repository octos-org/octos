import type { ProfileConfig, EmailSettings } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

const EMPTY_SMTP: EmailSettings = {
  provider: 'smtp',
  smtp_host: '',
  smtp_port: 465,
  username: '',
  password_env: 'SMTP_PASSWORD',
  from_address: '',
}

const EMPTY_FEISHU: EmailSettings = {
  provider: 'feishu',
  feishu_app_id: '',
  feishu_app_secret_env: 'FEISHU_APP_SECRET',
  feishu_from_address: '',
  feishu_region: 'cn',
}

export default function EmailTab({ config, onChange }: Props) {
  const email = config.email
  const enabled = !!email

  const update = (patch: Partial<EmailSettings>) => {
    onChange({ ...config, email: { ...(email ?? EMPTY_SMTP), ...patch } })
  }

  const toggleEnabled = () => {
    if (enabled) {
      onChange({ ...config, email: null })
    } else {
      onChange({ ...config, email: { ...EMPTY_SMTP } })
    }
  }

  const switchProvider = (provider: string) => {
    if (provider === 'smtp') {
      onChange({ ...config, email: { ...EMPTY_SMTP } })
    } else {
      onChange({ ...config, email: { ...EMPTY_FEISHU } })
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <p className="text-sm font-medium text-gray-300">Send Email Tool</p>
          <p className="text-xs text-gray-500">Allow the agent to send emails. On the admin profile, SMTP settings here also back dashboard OTP login email when a separate dashboard SMTP config is not set.</p>
        </div>
        <button
          onClick={toggleEnabled}
          className={`relative w-10 h-5 rounded-full transition-colors ${enabled ? 'bg-accent' : 'bg-gray-600'}`}
        >
          <span
            className={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white transition-transform ${enabled ? 'translate-x-5' : ''}`}
          />
        </button>
      </div>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Provider</label>
            <div className="flex gap-2">
              {['smtp', 'feishu'].map((p) => (
                <button
                  key={p}
                  onClick={() => switchProvider(p)}
                  className={`px-3 py-1.5 text-sm rounded-lg border transition-colors ${
                    email?.provider === p
                      ? 'border-accent text-accent bg-accent/10'
                      : 'border-gray-600 text-gray-400 hover:border-gray-500'
                  }`}
                >
                  {p === 'smtp' ? 'SMTP' : 'Feishu / Lark'}
                </button>
              ))}
            </div>
          </div>

          {email?.provider === 'smtp' && (
            <>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">SMTP Host</label>
                <input
                  value={email.smtp_host || ''}
                  onChange={(e) => update({ smtp_host: e.target.value })}
                  placeholder="smtp.gmail.com"
                  className="input"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">SMTP Port</label>
                <input
                  value={email.smtp_port ?? ''}
                  onChange={(e) => update({ smtp_port: e.target.value ? Number(e.target.value) : undefined })}
                  placeholder="465"
                  className="input max-w-[120px]"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Username</label>
                <input
                  value={email.username || ''}
                  onChange={(e) => update({ username: e.target.value })}
                  placeholder="user@gmail.com"
                  className="input"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Password Env Var</label>
                <input
                  value={email.password_env || ''}
                  onChange={(e) => update({ password_env: e.target.value })}
                  placeholder="SMTP_PASSWORD"
                  className="input"
                />
                <p className="text-xs text-gray-500 mt-1">
                  Name of the environment variable holding the SMTP password. Set the actual value in Environment Variables below.
                </p>
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">From Address</label>
                <input
                  value={email.from_address || ''}
                  onChange={(e) => update({ from_address: e.target.value })}
                  placeholder="bot@example.com"
                  className="input"
                />
              </div>
            </>
          )}

          {(email?.provider === 'feishu' || email?.provider === 'lark') && (
            <>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">App ID</label>
                <input
                  value={email.feishu_app_id || ''}
                  onChange={(e) => update({ feishu_app_id: e.target.value })}
                  placeholder="cli_xxxxxxxxxx"
                  className="input"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">App Secret Env Var</label>
                <input
                  value={email.feishu_app_secret_env || ''}
                  onChange={(e) => update({ feishu_app_secret_env: e.target.value })}
                  placeholder="FEISHU_APP_SECRET"
                  className="input"
                />
                <p className="text-xs text-gray-500 mt-1">
                  Name of the environment variable holding the Feishu app secret. Set the actual value in Environment Variables below.
                </p>
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">From Address</label>
                <input
                  value={email.feishu_from_address || ''}
                  onChange={(e) => update({ feishu_from_address: e.target.value })}
                  placeholder="sender@company.com"
                  className="input"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-300 mb-1.5">Region</label>
                <div className="flex gap-2">
                  {['cn', 'global'].map((r) => (
                    <button
                      key={r}
                      onClick={() => update({ feishu_region: r })}
                      className={`px-3 py-1.5 text-sm rounded-lg border transition-colors ${
                        (email.feishu_region || 'cn') === r
                          ? 'border-accent text-accent bg-accent/10'
                          : 'border-gray-600 text-gray-400 hover:border-gray-500'
                      }`}
                    >
                      {r === 'cn' ? 'China (feishu.cn)' : 'Global (larksuite.com)'}
                    </button>
                  ))}
                </div>
              </div>
            </>
          )}
        </>
      )}
    </div>
  )
}
