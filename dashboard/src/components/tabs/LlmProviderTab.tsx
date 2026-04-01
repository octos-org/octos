import { useState, useEffect, useCallback } from 'react'
import type { ProfileConfig, FallbackModel } from '../../types'
import { PROVIDERS } from '../../types'
import { myApi } from '../../api'
import _PROVIDER_MODELS from '../../providers.json'

interface ModelEntry {
  id: string
  input: number
  output: number
}

const PROVIDER_MODELS = _PROVIDER_MODELS as Record<string, { env: string; models: ModelEntry[] }>

// Provider registry handles all providers natively — no mapping needed.

function getApiKeyEnvName(provider: string | null | undefined): string {
  const entry = PROVIDER_MODELS[provider || '']
  return entry?.env || `${(provider || 'ANTHROPIC').toUpperCase()}_API_KEY`
}

function getModelIds(provider: string, fetched?: Record<string, string[]>): string[] {
  const staticIds = (PROVIDER_MODELS[provider]?.models || []).map((m) => m.id)
  const dynamicIds = fetched?.[provider] || []
  // Merge: static first, then any dynamic models not already in static
  const seen = new Set(staticIds)
  const merged = [...staticIds]
  for (const id of dynamicIds) {
    if (!seen.has(id)) {
      merged.push(id)
      seen.add(id)
    }
  }
  return merged
}

function getModelPricing(provider: string, modelId: string): ModelEntry | null {
  const entry = PROVIDER_MODELS[provider]
  if (!entry) return null
  return entry.models.find((m) => m.id === modelId) || null
}

function formatPrice(p: ModelEntry): string {
  if (p.input === 0 && p.output === 0) return 'Free (local)'
  return `$${p.input}/M in, $${p.output}/M out`
}

/** Generate a unique env var name for a fallback, avoiding collisions. */
function getFallbackEnvName(provider: string, index: number, allFallbacks: FallbackModel[], primaryEnv: string): string {
  const baseEnv = getApiKeyEnvName(provider)
  if (!baseEnv) return `FALLBACK_${index}_API_KEY`
  // Check if primary or another fallback already uses this env name
  const usedByPrimary = primaryEnv === baseEnv
  const usedByEarlierFallback = allFallbacks.some(
    (fb, i) => i < index && (fb.api_key_env || getApiKeyEnvName(fb.provider)) === baseEnv
  )
  // If no collision, use the base name (allows sharing keys between primary and fallback intentionally)
  if (!usedByPrimary && !usedByEarlierFallback) return baseEnv
  // Same provider used by primary — share the key (common: same provider, different model)
  if (usedByPrimary && !usedByEarlierFallback) return baseEnv
  // Collision with another fallback — suffix with index
  return `${baseEnv}_${index + 1}`
}

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  profileId?: string
}

type TestState = 'idle' | 'testing' | 'success' | 'error'

interface TestResult {
  state: TestState
  error: string
  pricing: ModelEntry | null
}

export default function LlmProviderTab({ config, onChange, profileId }: Props) {
  const primaryEnv = getApiKeyEnvName(config.provider)
  const fallbacks = config.fallback_models || []

  // Test results keyed by index (-1 = primary)
  const [testResults, setTestResults] = useState<Record<number, TestResult>>({})
  // Dynamic models fetched from provider APIs (keyed by provider name)
  const [fetchedModels, setFetchedModels] = useState<Record<string, string[]>>({})

  // Auto-fetch model lists from all configured providers on mount
  const fetchModelsForProvider = useCallback(async (provider: string, apiKeyEnv?: string | null) => {
    if (!provider || fetchedModels[provider]) return
    try {
      const models = await myApi.fetchProviderModels({
        provider,
        model: '',
        api_key_env: apiKeyEnv || getApiKeyEnvName(provider),
        profile_id: profileId,
      })
      if (models.length > 0) {
        setFetchedModels((s) => ({ ...s, [provider]: models }))
      }
    } catch {
      // silently ignore — provider may not support /v1/models
    }
  }, [fetchedModels])

  useEffect(() => {
    // Fetch for primary provider
    if (config.provider) {
      fetchModelsForProvider(config.provider, config.api_key_env)
    }
    // Fetch for each fallback provider
    for (const fb of fallbacks) {
      if (fb.provider) {
        fetchModelsForProvider(fb.provider, fb.api_key_env)
      }
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  const updateConfig = (patch: Partial<ProfileConfig>) => {
    onChange({ ...config, ...patch })
  }

  /** Change primary provider — updates provider, model, and api_key_env together. */
  const changePrimaryProvider = (provider: string | null) => {
    const models = getModelIds(provider || '', fetchedModels)
    const newEnv = getApiKeyEnvName(provider)
    updateConfig({ provider, model: models[0] || null, api_key_env: newEnv })
  }

  /** Change primary API key value. */
  const changePrimaryKey = (value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[primaryEnv] = value
    } else {
      delete newEnvVars[primaryEnv]
    }
    updateConfig({ api_key_env: primaryEnv, env_vars: newEnvVars })
  }

  const setFallbacks = (fbs: FallbackModel[]) => {
    updateConfig({ fallback_models: fbs })
  }

  const addFallback = () => {
    const provider = 'deepseek'
    const models = getModelIds(provider, fetchedModels)
    const env = getFallbackEnvName(provider, 0, fallbacks, primaryEnv)
    setFallbacks([{ provider, model: models[0] || null, api_key_env: env }, ...fallbacks])
    // Scroll to the new fallback after render
    setTimeout(() => document.getElementById('fallback-0')?.scrollIntoView({ behavior: 'smooth', block: 'center' }), 100)
  }

  /** Change fallback provider — updates provider, model, and api_key_env together. */
  const changeFallbackProvider = (idx: number, provider: string) => {
    const models = getModelIds(provider, fetchedModels)
    const newEnv = getFallbackEnvName(provider, idx, fallbacks, primaryEnv)
    updateFallback(idx, { provider, model: models[0] || null, api_key_env: newEnv })
  }

  const moveFallback = (idx: number, direction: -1 | 1) => {
    const target = idx + direction
    if (target < 0 || target >= fallbacks.length) return
    const updated = [...fallbacks]
    ;[updated[idx], updated[target]] = [updated[target], updated[idx]]
    setFallbacks(updated)
    // Swap test results too
    setTestResults((prev) => {
      const next = { ...prev }
      const a = prev[idx]
      const b = prev[target]
      if (a) next[target] = a; else delete next[target]
      if (b) next[idx] = b; else delete next[idx]
      return next
    })
  }

  const removeFallback = (idx: number) => {
    setFallbacks(fallbacks.filter((_, i) => i !== idx))
    // Clear test result for removed index
    setTestResults((prev) => {
      const next = { ...prev }
      delete next[idx]
      return next
    })
  }

  const updateFallback = (idx: number, patch: Partial<FallbackModel>) => {
    const updated = fallbacks.map((fb, i) => (i === idx ? { ...fb, ...patch } : fb))
    updateConfig({ fallback_models: updated })
  }

  const updateFallbackEnvVar = (idx: number, fbEnv: string, value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[fbEnv] = value
    } else {
      delete newEnvVars[fbEnv]
    }
    const updated = fallbacks.map((fb, i) => (i === idx ? { ...fb, api_key_env: fbEnv } : fb))
    updateConfig({ env_vars: newEnvVars, fallback_models: updated })
  }

  const doTest = async (key: number, provider: string, model: string, apiKeyEnv: string) => {
    const apiKey = config.env_vars[apiKeyEnv] || ''
    if (!apiKey) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: 'No API key configured.', pricing: null } }))
      return
    }
    if (!model) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: 'No model selected.', pricing: null } }))
      return
    }
    setTestResults((s) => ({ ...s, [key]: { state: 'testing', error: '', pricing: null } }))
    try {
      const isMasked = apiKey.includes('***')
      const res = await myApi.testProvider({
        provider,
        model,
        // If key is masked (loaded from server), send env name so backend reads from saved profile
        // If key is fresh (user just typed it), send the raw key
        api_key: isMasked ? undefined : apiKey,
        api_key_env: isMasked ? apiKeyEnv : undefined,
      })
      const pricing = getModelPricing(provider, model)
      if (res.ok) {
        setTestResults((s) => ({ ...s, [key]: { state: 'success', error: '', pricing } }))
        // Store dynamically fetched models from the provider
        if (res.models && res.models.length > 0) {
          setFetchedModels((s) => ({ ...s, [provider]: res.models! }))
        }
      } else {
        setTestResults((s) => ({ ...s, [key]: { state: 'error', error: res.error || 'Unknown error', pricing: null } }))
      }
    } catch (e: unknown) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: e instanceof Error ? e.message : 'Request failed', pricing: null } }))
    }
  }

  return (
    <div className="space-y-6">
      <div className="bg-amber-500/10 border border-amber-500/20 rounded-lg p-3 text-xs text-amber-400">
        LLM provider is required to start the gateway. Configure a primary provider and optional fallbacks for automatic failover.
      </div>

      {/* ── Primary Provider ── */}
      <div className="bg-surface-dark/30 rounded-lg p-4 border border-gray-700/50 space-y-4">
        <h3 className="text-sm font-semibold text-gray-200">Primary Provider</h3>

        <Field label="Provider">
          <select
            value={config.provider || ''}
            onChange={(e) => changePrimaryProvider(e.target.value || null)}
            className="input"
          >
            {!config.provider && <option value="">Select a provider...</option>}
            {PROVIDERS.map((p) => (
              <option key={p} value={p}>{p}</option>
            ))}
          </select>
        </Field>

        <ModelSelect
          provider={config.provider || ''}
          model={config.model || ''}
          onModelChange={(model) => updateConfig({ model })}
          fetchedModels={fetchedModels}
        />

        <Field label="API Key" hint={primaryEnv ? `Stored as ${primaryEnv}` : undefined}>
          <input
            type="password"
            value={config.env_vars[primaryEnv] || ''}
            onChange={(e) => changePrimaryKey(e.target.value)}
            placeholder={`Paste your ${config.provider || 'anthropic'} API key`}
            className="input font-mono text-xs"
          />
        </Field>

        <TestButton
          result={testResults[-1] || null}
          onTest={() => doTest(-1, config.provider || 'anthropic', config.model || '', primaryEnv)}
        />
      </div>

      {/* ── Fallback Models ── */}
      <div className="space-y-3">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-semibold text-gray-200">Fallback Models</h3>
          <button
            type="button"
            onClick={addFallback}
            className="px-3 py-1 text-xs font-medium rounded-lg bg-white/5 text-gray-400 hover:bg-white/10 hover:text-white border border-gray-700/50 transition"
          >
            + Add Fallback
          </button>
        </div>

        {fallbacks.length === 0 && (
          <p className="text-xs text-gray-600 italic">
            No fallback models configured. If the primary provider fails (429, 5xx, auth error), the gateway will retry the same provider.
          </p>
        )}

        {fallbacks.map((fb, idx) => {
          const fbEnv = fb.api_key_env || getApiKeyEnvName(fb.provider)
          const sharesPrimaryKey = fbEnv === primaryEnv
          return (
            <div
              key={idx}
              id={`fallback-${idx}`}
              className="bg-surface-dark/30 rounded-lg p-4 border border-gray-700/50 space-y-3"
            >
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-1.5">
                  <span className="text-xs font-medium text-gray-400">Fallback #{idx + 1}</span>
                  <button
                    type="button"
                    onClick={() => moveFallback(idx, -1)}
                    disabled={idx === 0}
                    className="p-0.5 text-gray-500 hover:text-gray-300 transition disabled:opacity-25 disabled:cursor-not-allowed"
                    title="Move up"
                  >
                    <ArrowUpIcon />
                  </button>
                  <button
                    type="button"
                    onClick={() => moveFallback(idx, 1)}
                    disabled={idx === fallbacks.length - 1}
                    className="p-0.5 text-gray-500 hover:text-gray-300 transition disabled:opacity-25 disabled:cursor-not-allowed"
                    title="Move down"
                  >
                    <ArrowDownIcon />
                  </button>
                </div>
                <button
                  type="button"
                  onClick={() => removeFallback(idx)}
                  className="p-1 text-red-400 hover:text-red-300 transition"
                  title="Remove fallback"
                >
                  <XIcon />
                </button>
              </div>

              <Field label="Provider">
                <select
                  value={fb.provider}
                  onChange={(e) => changeFallbackProvider(idx, e.target.value)}
                  className="input"
                >
                  {PROVIDERS.map((p) => (
                    <option key={p} value={p}>{p}</option>
                  ))}
                </select>
              </Field>

              <ModelSelect
                provider={fb.provider}
                model={fb.model || ''}
                onModelChange={(model) => updateFallback(idx, { model })}
                fetchedModels={fetchedModels}
              />

              <Field label="API Key" hint={sharesPrimaryKey ? `Shared with primary (${fbEnv})` : `Stored as ${fbEnv}`}>
                <input
                  type="password"
                  value={config.env_vars[fbEnv] || ''}
                  onChange={(e) => updateFallbackEnvVar(idx, fbEnv, e.target.value)}
                  placeholder={sharesPrimaryKey ? 'Using primary API key' : `Paste your ${fb.provider} API key`}
                  className="input font-mono text-xs"
                />
              </Field>

              <TestButton
                result={testResults[idx] || null}
                onTest={() => doTest(idx, fb.provider, fb.model || '', fbEnv)}
              />
            </div>
          )
        })}

        {fallbacks.length > 0 && (
          <p className="text-xs text-gray-600">
            Failover order: primary → fallback #1 → #2 → ... Providers with 3+ consecutive failures are temporarily skipped.
          </p>
        )}
      </div>
    </div>
  )
}

// ── Sub-components ──────────────────────────────────────────────────

function ModelSelect({ provider, model, onModelChange, fetchedModels }: { provider: string; model: string; onModelChange: (m: string) => void; fetchedModels?: Record<string, string[]> }) {
  const entry = PROVIDER_MODELS[provider || '']
  const staticModels = entry?.models || []
  const staticIds = staticModels.map((m) => m.id)
  // Merge static + dynamically fetched models
  const dynamicIds = (fetchedModels?.[provider] || []).filter((id) => !staticIds.includes(id))
  const allIds = [...staticIds, ...dynamicIds]
  const isCustom = model !== '' && !allIds.includes(model)
  const pricing = staticModels.find((m) => m.id === model)

  return (
    <Field label="Model">
      <div className="space-y-2">
        <select
          value={isCustom ? '__custom__' : model}
          onChange={(e) => {
            if (e.target.value === '__custom__') {
              onModelChange('')
            } else {
              onModelChange(e.target.value)
            }
          }}
          className="input"
        >
          {!model && <option value="">Select a model...</option>}
          {staticModels.map((m) => (
            <option key={m.id} value={m.id}>
              {m.id} — {m.input === 0 && m.output === 0 ? 'Free' : `$${m.input}/$${m.output} per 1M tokens`}
            </option>
          ))}
          {dynamicIds.length > 0 && <option disabled>── fetched from API ──</option>}
          {dynamicIds.map((id) => (
            <option key={id} value={id}>{id}</option>
          ))}
          <option value="__custom__">{isCustom ? `Custom: ${model}` : 'Custom model...'}</option>
        </select>
        {(isCustom || (!model && allIds.length === 0)) && (
          <input
            value={model}
            onChange={(e) => onModelChange(e.target.value)}
            placeholder="Enter model name"
            className="input text-xs font-mono"
            autoFocus
          />
        )}
        {pricing && pricing.input > 0 && (
          <p className="text-xs text-gray-500">
            {formatPrice(pricing)}
          </p>
        )}
      </div>
    </Field>
  )
}

function TestButton({ result, onTest }: { result: TestResult | null; onTest: () => void }) {
  const state = result?.state || 'idle'
  return (
    <div className="space-y-1">
      <button
        type="button"
        onClick={onTest}
        disabled={state === 'testing'}
        className={`px-3 py-1.5 text-xs font-medium rounded-lg transition flex items-center gap-1.5 ${
          state === 'success'
            ? 'bg-green-500/15 text-green-400 border border-green-500/30'
            : state === 'error'
              ? 'bg-red-500/15 text-red-400 border border-red-500/30'
              : 'bg-white/5 text-gray-400 hover:bg-white/10 hover:text-white border border-gray-700/50'
        } disabled:opacity-50`}
      >
        {state === 'testing' && <Spinner />}
        {state === 'success' && <CheckIcon />}
        {state === 'error' && <AlertIcon />}
        {state === 'testing' ? 'Testing...' : state === 'success' ? 'Connected' : state === 'error' ? 'Failed — Retry' : 'Test Connection'}
      </button>
      {state === 'success' && result?.pricing && (
        <p className="text-xs text-green-400/80 pl-1">
          {formatPrice(result.pricing)}
        </p>
      )}
      {state === 'error' && result?.error && (
        <p className="text-xs text-red-400/80 pl-1">{result.error}</p>
      )}
    </div>
  )
}

function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div>
      <label className="block text-sm font-medium text-gray-300 mb-1.5">{label}</label>
      {hint && <p className="text-xs text-gray-500 mb-1.5">{hint}</p>}
      {children}
    </div>
  )
}

// ── Icons ───────────────────────────────────────────────────────────

function XIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M6 18L18 6M6 6l12 12" />
    </svg>
  )
}

function Spinner() {
  return (
    <svg className="w-3 h-3 animate-spin" viewBox="0 0 24 24" fill="none">
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
    </svg>
  )
}

function CheckIcon() {
  return (
    <svg className="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={3}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M5 13l4 4L19 7" />
    </svg>
  )
}

function AlertIcon() {
  return (
    <svg className="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M12 9v2m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
    </svg>
  )
}

function ArrowUpIcon() {
  return (
    <svg className="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M5 15l7-7 7 7" />
    </svg>
  )
}

function ArrowDownIcon() {
  return (
    <svg className="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M19 9l-7 7-7-7" />
    </svg>
  )
}
