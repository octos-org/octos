import { useState } from 'react'
import type { ProfileConfig, SearchProviderId } from '../../types'
import { myApi } from '../../api'

const SEARCH_PROVIDERS = [
  {
    id: 'tavily',
    defaultEnvKey: 'TAVILY_API_KEY',
    label: 'Tavily',
    placeholder: 'tvly-...',
    description: 'AI-optimized search, highest priority provider (1k free/month)',
    link: 'https://app.tavily.com/home',
  },
  {
    id: 'perplexity',
    defaultEnvKey: 'PERPLEXITY_API_KEY',
    label: 'Perplexity',
    placeholder: 'pplx-...',
    description: 'AI-powered search (Sonar model)',
    link: 'https://www.perplexity.ai/settings/api',
  },
  {
    id: 'brave',
    defaultEnvKey: 'BRAVE_API_KEY',
    label: 'Brave Search',
    placeholder: 'BSA...',
    description: 'Independent web index, free tier: 2k queries/month',
    link: 'https://brave.com/search/api/',
  },
  {
    id: 'serper',
    defaultEnvKey: 'SERPER_API_KEY',
    label: 'Serper',
    placeholder: '',
    description: 'Google SERP API for web, news, and local search',
    link: 'https://serper.dev/',
  },
  {
    id: 'you',
    defaultEnvKey: 'YDC_API_KEY',
    label: 'You.com',
    placeholder: '',
    description: 'Rich JSON results with snippets',
    link: 'https://you.com/search?q=api',
  },
] as const satisfies ReadonlyArray<{
  id: SearchProviderId
  defaultEnvKey: string
  label: string
  placeholder: string
  description: string
  link: string
}>

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

type TestState = 'idle' | 'testing' | 'success' | 'error'

interface TestResult {
  state: TestState
  error: string
}

export default function SearchApiTab({ config, onChange }: Props) {
  const [testResults, setTestResults] = useState<Record<string, TestResult>>({})

  const getApiKeyEnvRef = (providerId: SearchProviderId, legacyEnvKey: string) => {
    const explicit = config.search?.providers?.[providerId]?.api_key_env?.trim()
    if (explicit) return explicit
    return config.env_vars?.[legacyEnvKey] ? legacyEnvKey : ''
  }

  const updateProvider = (providerId: SearchProviderId, apiKeyEnv: string) => {
    const nextProviders = { ...(config.search?.providers ?? {}) }
    const normalized = apiKeyEnv.trim()

    if (normalized) {
      nextProviders[providerId] = { ...(nextProviders[providerId] ?? {}), api_key_env: normalized }
    } else {
      delete nextProviders[providerId]
    }

    onChange({
      ...config,
      search:
        Object.keys(nextProviders).length > 0
          ? { ...(config.search ?? {}), providers: nextProviders }
          : undefined,
    })
  }

  const doTest = async (providerId: string, envKey: string) => {
    setTestResults((prev) => ({ ...prev, [providerId]: { state: 'testing', error: '' } }))

    try {
      const apiKeyEnv = envKey.trim()

      const res = await myApi.testSearch({
        provider: providerId,
        api_key_env: apiKeyEnv || undefined,
      })

      if (res.ok) {
        setTestResults((prev) => ({ ...prev, [providerId]: { state: 'success', error: '' } }))
      } else {
        setTestResults((prev) => ({
          ...prev,
          [providerId]: { state: 'error', error: res.error || 'Unknown error' },
        }))
      }
    } catch (e: unknown) {
      setTestResults((prev) => ({
        ...prev,
        [providerId]: { state: 'error', error: e instanceof Error ? e.message : 'Request failed' },
      }))
    }
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Web Search APIs</p>
        <p>
          Bind each provider to a secret env var for the{' '}
          <code className="bg-gray-800 px-1 rounded">web_search</code> tool. DuckDuckGo is used by
          default with no API key. Product behavior now lives in{' '}
          <code className="bg-gray-800 px-1 rounded">config.search</code>; this tab only stores the
          secret reference, not the key value itself.
        </p>
      </div>

      {SEARCH_PROVIDERS.map(({ id, defaultEnvKey, label, placeholder, description, link }) => {
        const test = testResults[id] || { state: 'idle', error: '' }
        const envRef = getApiKeyEnvRef(id, defaultEnvKey)
        const explicitBinding = config.search?.providers?.[id]?.api_key_env?.trim() || ''
        const inferredLegacyBinding = !explicitBinding && !!config.env_vars?.[defaultEnvKey]

        return (
          <div key={id} className="bg-surface-dark/30 rounded-lg p-3 border border-gray-700/40">
            <div className="flex items-center justify-between mb-1.5">
              <label className="text-sm font-medium text-gray-300">{label}</label>
              <a
                href={link}
                target="_blank"
                rel="noopener"
                className="text-[10px] text-accent hover:underline"
              >
                Get API key
              </a>
            </div>
            <p className="text-[10px] text-gray-500 mb-2">{description}</p>

            <div className="flex gap-2 items-start">
              <div className="flex-1">
                <input
                  value={envRef}
                  onChange={(e) => updateProvider(id, e.target.value)}
                  placeholder={defaultEnvKey}
                  className="input font-mono text-xs w-full"
                />
                <p className="text-[10px] text-gray-600 mt-1">
                  Secret env var reference
                  {placeholder ? ` • expected key format ${placeholder}` : ''}
                </p>
                {inferredLegacyBinding && (
                  <p className="text-[10px] text-amber-400 mt-1">
                    Using legacy binding from Env Vars. Save once to persist it into{' '}
                    <code className="bg-gray-800 px-1 rounded">config.search</code>.
                  </p>
                )}
              </div>

              <button
                onClick={() => doTest(id, envRef)}
                disabled={!envRef || test.state === 'testing'}
                className={`mt-0.5 px-3 py-1.5 rounded text-xs font-medium transition-colors shrink-0 ${
                  test.state === 'success'
                    ? 'bg-green-600/80 text-white'
                    : test.state === 'error'
                      ? 'bg-red-600/80 text-white'
                      : 'bg-accent/80 text-white hover:bg-accent disabled:opacity-40 disabled:cursor-not-allowed'
                }`}
              >
                {test.state === 'testing'
                  ? 'Testing...'
                  : test.state === 'success'
                    ? 'Connected'
                    : test.state === 'error'
                      ? 'Failed'
                      : 'Test'}
              </button>
            </div>

            {test.state === 'error' && test.error && (
              <p className="text-[10px] text-red-400 mt-1.5 break-all">{test.error}</p>
            )}
          </div>
        )
      })}
    </div>
  )
}
