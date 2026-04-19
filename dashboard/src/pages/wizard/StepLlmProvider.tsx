import { useState } from 'react'
import { api } from '../../api'

type ProviderChoice = {
  id: string
  label: string
  defaultModel: string
  needsBaseUrl?: boolean
}

const PROVIDERS: ProviderChoice[] = [
  { id: 'anthropic', label: 'Anthropic', defaultModel: 'claude-sonnet-4-6' },
  { id: 'openai', label: 'OpenAI', defaultModel: 'gpt-5.1' },
  { id: 'gemini', label: 'Gemini', defaultModel: 'gemini-2.5-flash' },
  { id: 'openrouter', label: 'OpenRouter', defaultModel: 'anthropic/claude-sonnet-4-6' },
  { id: 'openai-compatible', label: 'OpenAI-compatible', defaultModel: 'gpt-4o-mini', needsBaseUrl: true },
]

type TestState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'ok'; message: string }
  | { kind: 'err'; error: string }

export default function StepLlmProvider() {
  const [providerId, setProviderId] = useState(PROVIDERS[0].id)
  const provider = PROVIDERS.find((p) => p.id === providerId) ?? PROVIDERS[0]
  const [apiKey, setApiKey] = useState('')
  const [model, setModel] = useState(provider.defaultModel)
  const [baseUrl, setBaseUrl] = useState('')
  const [test, setTest] = useState<TestState>({ kind: 'idle' })

  const handleProviderChange = (id: string) => {
    setProviderId(id)
    const next = PROVIDERS.find((p) => p.id === id)
    if (next) {
      setModel(next.defaultModel)
    }
    setTest({ kind: 'idle' })
  }

  const handleTest = async () => {
    setTest({ kind: 'loading' })
    try {
      const res = await api.testProvider({
        provider: provider.id === 'openai-compatible' ? (baseUrl || 'openai-compatible') : provider.id,
        model,
        api_key: apiKey,
        base_url: provider.needsBaseUrl ? baseUrl || undefined : undefined,
      })
      if (res.ok) {
        setTest({ kind: 'ok', message: res.message || 'Connection verified.' })
      } else {
        setTest({ kind: 'err', error: res.error || 'Test failed.' })
      }
    } catch (e: any) {
      setTest({ kind: 'err', error: e?.message || 'Test request failed.' })
    }
  }

  const canTest = apiKey.trim().length > 0 && model.trim().length > 0 && !(provider.needsBaseUrl && !baseUrl.trim())

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">LLM provider</h2>
        <p className="text-sm text-gray-400">
          Pick a default model provider and verify that your API key works.
        </p>
      </div>

      <div>
        <label className="block text-xs font-medium text-gray-400 mb-1">Provider</label>
        <select
          value={providerId}
          onChange={(e) => handleProviderChange(e.target.value)}
          className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white focus:outline-none focus:border-accent"
        >
          {PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      </div>

      <div>
        <label className="block text-xs font-medium text-gray-400 mb-1">Model</label>
        <input
          type="text"
          value={model}
          onChange={(e) => setModel(e.target.value)}
          className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
        />
      </div>

      {provider.needsBaseUrl && (
        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">Base URL</label>
          <input
            type="text"
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="https://api.example.com/v1"
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
      )}

      <div>
        <label className="block text-xs font-medium text-gray-400 mb-1">API key</label>
        <input
          type="password"
          value={apiKey}
          onChange={(e) => setApiKey(e.target.value)}
          autoComplete="off"
          className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
        />
      </div>

      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={handleTest}
          disabled={!canTest || test.kind === 'loading'}
          className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
        >
          {test.kind === 'loading' ? 'Testing…' : 'Test connection'}
        </button>
        {test.kind === 'ok' && (
          <span className="text-sm text-green-400">✓ {test.message}</span>
        )}
        {test.kind === 'err' && (
          <span className="text-sm text-red-400 break-all">✗ {test.error}</span>
        )}
      </div>

      <p className="text-xs text-gray-500 border-t border-gray-700/50 pt-3">
        Configured providers are saved when you create your first profile (Profiles → New).
        This step only verifies that the credentials work.
      </p>
    </div>
  )
}
