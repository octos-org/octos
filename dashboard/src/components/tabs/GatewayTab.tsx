import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function GatewayTab({ config, onChange }: Props) {
  const updateGateway = (field: string, value: number | string | null) => {
    onChange({
      ...config,
      gateway: { ...config.gateway, [field]: value },
    })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Gateway Settings</p>
        <p>Configure the agent's behavior for this gateway instance. These settings control conversation memory, tool usage limits, and the system prompt that shapes the agent's personality.</p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max History</label>
        <input
          type="number"
          value={config.gateway.max_history ?? ''}
          onChange={(e) =>
            updateGateway('max_history', e.target.value ? Number(e.target.value) : null)
          }
          placeholder="50"
          className="input max-w-[200px]"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Maximum number of messages to keep in conversation history
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Iterations</label>
        <input
          type="number"
          value={config.gateway.max_iterations ?? ''}
          onChange={(e) =>
            updateGateway('max_iterations', e.target.value ? Number(e.target.value) : null)
          }
          placeholder="50"
          className="input max-w-[200px]"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Maximum tool-call iterations per agent turn
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Concurrent Sessions</label>
        <input
          type="number"
          value={config.gateway.max_concurrent_sessions ?? ''}
          onChange={(e) =>
            updateGateway('max_concurrent_sessions', e.target.value ? Number(e.target.value) : null)
          }
          placeholder="10"
          className="input max-w-[200px]"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Maximum number of concurrent chat sessions (default: unlimited)
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Browser Timeout (seconds)</label>
        <input
          type="number"
          value={config.gateway.browser_timeout_secs ?? ''}
          onChange={(e) =>
            updateGateway('browser_timeout_secs', e.target.value ? Number(e.target.value) : null)
          }
          placeholder="30"
          className="input max-w-[200px]"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Timeout for headless browser tool operations
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Output Tokens</label>
        <input
          type="number"
          value={config.gateway.max_output_tokens ?? ''}
          onChange={(e) =>
            updateGateway('max_output_tokens', e.target.value ? Number(e.target.value) : null)
          }
          placeholder="4096"
          className="input max-w-[200px]"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Default max output tokens per LLM call. Higher values allow longer responses but cost more. Pipeline nodes can override this per-node.
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">System Prompt</label>
        <textarea
          value={config.gateway.system_prompt ?? ''}
          onChange={(e) => updateGateway('system_prompt', e.target.value || null)}
          placeholder="You are a helpful assistant."
          rows={4}
          className="input"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Custom system prompt for this gateway instance
        </p>
      </div>
    </div>
  )
}
