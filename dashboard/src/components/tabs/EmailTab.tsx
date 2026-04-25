import type { ProfileConfig, EmailSettings } from '../../types'
import SmtpFields from '../SmtpFields'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

const EMPTY_SMTP: EmailSettings = {
  provider: 'smtp',
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
          <p className="text-xs text-gray-500">
            Allow the agent to send emails. The SMTP provider uses the dashboard SMTP settings
            (shared across all profiles and the OTP login flow).
          </p>
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
            <div className="border border-gray-700/50 rounded-lg p-4 bg-background/40">
              <p className="text-xs text-gray-500 mb-3">
                Edits below save to the dashboard-wide SMTP store and take effect immediately —
                they apply to OTP login emails and to all profiles using the SMTP email provider.
              </p>
              <SmtpFields />
            </div>
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
