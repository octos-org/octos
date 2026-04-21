import { useState, useEffect, useCallback } from 'react'
import type {
  ProfileConfig,
  LlmModelSelectionConfig,
  LlmRouteConfig,
} from '../../types'
import { myApi } from '../../api'
import _PROVIDER_MODELS from '../../providers.json'

const CUSTOM_FAMILY = '__custom_family__'
const CUSTOM_MODEL = '__custom_model__'
const CUSTOM_ROUTE = '__custom_route__'
const OFFICIAL_ROUTE_ID = 'official'

/** An API host that serves one or more models. Empty base_url = family default. */
interface ModelEndpoint {
  id: string
  label: string
  base_url?: string
  api_key_env?: string
}

interface ModelEntry {
  id: string
  input: number
  output: number
  endpoints?: ModelEndpoint[]
}

const PROVIDER_MODELS = _PROVIDER_MODELS as Record<string, { env: string; models: ModelEntry[] }>
const PROVIDER_FAMILIES = Object.keys(PROVIDER_MODELS)

function isKnownProvider(provider: string | null | undefined): boolean {
  return !!provider && Object.prototype.hasOwnProperty.call(PROVIDER_MODELS, provider)
}

function providerRouteKey(provider: string | null | undefined, baseUrl?: string | null): string {
  return `${provider || ''}::${baseUrl || ''}`
}

function slugifyRoutePart(value: string | null | undefined): string {
  return (value || '')
    .trim()
    .replace(/[^a-zA-Z0-9]+/g, '_')
    .replace(/^_+|_+$/g, '')
    .toUpperCase()
}

function defaultCustomRouteId(label: string | null | undefined): string {
  const slug = slugifyRoutePart(label).toLowerCase()
  return slug || 'custom'
}

function defaultCustomRouteApiKeyEnv(label: string | null | undefined): string {
  const slug = slugifyRoutePart(label)
  return `${slug || 'CUSTOM'}_API_KEY`
}

// Provider registry handles all providers natively — no mapping needed.

function getApiKeyEnvName(provider: string | null | undefined): string {
  const entry = PROVIDER_MODELS[provider || '']
  return entry?.env || `${(provider || 'ANTHROPIC').toUpperCase()}_API_KEY`
}

/** Find a model entry in the family's catalogue by id. */
function findModelEntry(provider: string | null | undefined, modelId: string | null | undefined): ModelEntry | undefined {
  if (!provider || !modelId) return undefined
  return PROVIDER_MODELS[provider]?.models.find((m) => m.id === modelId)
}

/** Find the endpoint in `model.endpoints` that matches (base_url, api_key_env). */
function findMatchingEndpoint(
  model: ModelEntry | undefined,
  route: LlmRouteConfig | null | undefined,
): ModelEndpoint | undefined {
  if (!model?.endpoints) return undefined
  if (route?.route_id) {
    const byId = model.endpoints.find((e) => e.id === route.route_id)
    if (byId) return byId
  }
  return model.endpoints.find((e) =>
    (e.base_url || '') === (route?.base_url || '') &&
    (!e.api_key_env || e.api_key_env === route?.api_key_env)
  )
}

function getModelIds(provider: string, fetched?: Record<string, string[]>, baseUrl?: string | null): string[] {
  const staticIds = (PROVIDER_MODELS[provider]?.models || []).map((m) => m.id)
  const dynamicIds = fetched?.[providerRouteKey(provider, baseUrl)] || []
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

function formatEndpointSummary(model: ModelEntry | undefined): string | null {
  const endpoints = model?.endpoints || []
  if (endpoints.length === 0) return null
  return endpoints.map((ep) => ep.label).join(', ')
}

function nullable(value: string | null | undefined): string | null {
  return value && value.length > 0 ? value : null
}

function buildRouteFromEndpoint(
  provider: string | null | undefined,
  endpoint: ModelEndpoint | undefined,
): LlmRouteConfig {
  return {
    route_id: endpoint?.id || (provider ? OFFICIAL_ROUTE_ID : null),
    label: endpoint?.label || (provider ? 'Official API' : null),
    base_url: nullable(endpoint?.base_url),
    api_key_env: nullable(endpoint?.api_key_env || getApiKeyEnvName(provider)),
    api_type: null,
  }
}

function hasCustomRoute(
  provider: string | null | undefined,
  modelId: string | null | undefined,
  route: LlmRouteConfig | null | undefined,
): boolean {
  if (!route) return false
  const model = findModelEntry(provider, modelId)
  if (findMatchingEndpoint(model, route)) return false
  const defaultEnv = getApiKeyEnvName(provider)
  return !!(
    route.base_url ||
    route.label ||
    (route.route_id && route.route_id !== OFFICIAL_ROUTE_ID) ||
    (route.api_key_env && route.api_key_env !== defaultEnv)
  )
}

function buildCustomRoute(
  route: LlmRouteConfig | null | undefined,
): LlmRouteConfig {
  const previousLabel = nullable(route?.label)
  const nextLabel = previousLabel || 'Custom route'
  const previousEnv = nullable(route?.api_key_env)
  const defaultPreviousEnv = defaultCustomRouteApiKeyEnv(previousLabel)
  const nextEnv = previousEnv && previousEnv !== defaultPreviousEnv
    ? previousEnv
    : defaultCustomRouteApiKeyEnv(nextLabel)
  return {
    route_id: nullable(route?.route_id) || defaultCustomRouteId(nextLabel),
    label: nextLabel,
    base_url: nullable(route?.base_url),
    api_key_env: nextEnv,
    api_type: route?.api_type || null,
  }
}

function buildRouteForModel(
  provider: string | null | undefined,
  modelId: string | null | undefined,
  route: LlmRouteConfig | null | undefined,
): LlmRouteConfig {
  const model = findModelEntry(provider, modelId)
  const matched = findMatchingEndpoint(model, route)
  if (matched) return buildRouteFromEndpoint(provider, matched)
  if (hasCustomRoute(provider, modelId, route)) return buildCustomRoute(route)
  return buildRouteFromEndpoint(provider, model?.endpoints?.[0])
}

function patchCustomRoute(
  route: LlmRouteConfig | null | undefined,
  patch: Partial<LlmRouteConfig>,
): LlmRouteConfig {
  const next = buildCustomRoute(route)
  const previousLabel = nullable(next.label)
  const label = nullable(
    patch.label !== undefined ? patch.label : next.label,
  ) || 'Custom route'
  const envWasAuto = next.api_key_env === defaultCustomRouteApiKeyEnv(previousLabel)
  const apiKeyEnv = nullable(
    patch.api_key_env !== undefined
      ? patch.api_key_env
      : envWasAuto
        ? defaultCustomRouteApiKeyEnv(label)
        : next.api_key_env,
  ) || defaultCustomRouteApiKeyEnv(label)
  const routeId = nullable(
    patch.route_id !== undefined
      ? patch.route_id
      : next.route_id === defaultCustomRouteId(previousLabel)
        ? defaultCustomRouteId(label)
        : next.route_id,
  ) || defaultCustomRouteId(label)
  return {
    ...next,
    ...patch,
    route_id: routeId,
    label,
    api_key_env: apiKeyEnv,
    base_url: nullable(
      patch.base_url !== undefined ? patch.base_url : next.base_url,
    ),
  }
}

function getRouteLabel(
  provider: string | null | undefined,
  modelId: string | null | undefined,
  route: LlmRouteConfig | null | undefined,
): string {
  const model = findModelEntry(provider, modelId)
  return (
    findMatchingEndpoint(model, route)?.label ||
    nullable(route?.label) ||
    'Official API'
  )
}

function getPrimarySelection(config: ProfileConfig): LlmModelSelectionConfig {
  return (
    config.llm?.primary || {
      family_id: 'anthropic',
      model_id: 'claude-sonnet-4-20250514',
      route: {
        route_id: OFFICIAL_ROUTE_ID,
        label: 'Official API',
        base_url: null,
        api_key_env: 'ANTHROPIC_API_KEY',
        api_type: null,
      },
    }
  )
}

function getFallbackSelections(config: ProfileConfig): LlmModelSelectionConfig[] {
  return config.llm?.fallbacks || []
}

function applySelections(
  config: ProfileConfig,
  primary: LlmModelSelectionConfig,
  fallbacks: LlmModelSelectionConfig[],
): ProfileConfig {
  return {
    ...config,
    llm: {
      primary,
      fallbacks,
    },
  }
}

/** Generate a unique env var name for a fallback, avoiding collisions. */
function getFallbackEnvName(provider: string, index: number, allFallbacks: LlmModelSelectionConfig[], primaryEnv: string): string {
  const baseEnv = getApiKeyEnvName(provider)
  if (!baseEnv) return `FALLBACK_${index}_API_KEY`
  // Check if primary or another fallback already uses this env name
  const usedByPrimary = primaryEnv === baseEnv
  const usedByEarlierFallback = allFallbacks.some(
    (fb, i) =>
      i < index && ((fb.route?.api_key_env) || getApiKeyEnvName(fb.family_id)) === baseEnv
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
  const primary = getPrimarySelection(config)
  const primaryProvider = primary.family_id || ''
  const primaryModel = primary.model_id || ''
  const primaryBaseUrl = primary.route?.base_url || ''
  const primaryRouteLabel = getRouteLabel(primaryProvider, primaryModel, primary.route)
  const primaryHasCustomRoute = hasCustomRoute(primaryProvider, primaryModel, primary.route)
  const primaryEnv = primary.route?.api_key_env || getApiKeyEnvName(primaryProvider)
  const fallbacks = getFallbackSelections(config)

  // Test results keyed by index (-1 = primary)
  const [testResults, setTestResults] = useState<Record<number, TestResult>>({})
  // Dynamic models fetched from provider APIs, keyed by provider family + route.
  const [fetchedModels, setFetchedModels] = useState<Record<string, string[]>>({})

  const fetchModelsForProvider = useCallback(async (provider: string, apiKeyEnv?: string | null, baseUrl?: string | null) => {
    const routeKey = providerRouteKey(provider, baseUrl)
    if (!provider || fetchedModels[routeKey]) return
    try {
      const models = await myApi.fetchProviderModels({
        provider,
        model: '',
        api_key_env: apiKeyEnv || getApiKeyEnvName(provider),
        base_url: baseUrl || undefined,
        profile_id: profileId,
      })
      if (models.length > 0) {
        setFetchedModels((s) => ({ ...s, [routeKey]: models }))
      }
    } catch {
      // silently ignore — provider may not support /v1/models
    }
  }, [fetchedModels, profileId])

  useEffect(() => {
    if (
      primaryProvider &&
      (!primaryHasCustomRoute || primaryBaseUrl)
    ) {
      fetchModelsForProvider(primaryProvider, primaryEnv, primaryBaseUrl)
    }
    for (const fb of fallbacks) {
      if (
        fb.family_id &&
        (!hasCustomRoute(fb.family_id, fb.model_id, fb.route) || fb.route?.base_url)
      ) {
        fetchModelsForProvider(
          fb.family_id,
          fb.route?.api_key_env || getApiKeyEnvName(fb.family_id),
          fb.route?.base_url,
        )
      }
    }
  }, [fetchModelsForProvider, fallbacks, primaryBaseUrl, primaryEnv, primaryHasCustomRoute, primaryProvider])

  const updateSelections = (
    nextPrimary: LlmModelSelectionConfig,
    nextFallbacks: LlmModelSelectionConfig[],
  ) => {
    onChange(applySelections(config, nextPrimary, nextFallbacks))
  }

  const updateConfig = (patch: Partial<ProfileConfig>) => {
    onChange({ ...applySelections(config, primary, fallbacks), ...patch })
  }

  /** Change primary provider — updates provider, model, and api_key_env together.
   * Picks the first model and its first route (if any) as defaults. */
  const changePrimaryProvider = (provider: string | null) => {
    const modelId = getModelIds(provider || '', fetchedModels)[0] || null
    updateSelections(
      {
        ...primary,
        family_id: provider,
        model_id: modelId,
        route: buildRouteForModel(provider, modelId, null),
      },
      fallbacks,
    )
  }

  /** Change primary model while preserving the current custom or matching route when possible. */
  const changePrimaryModel = (modelId: string) => {
    updateSelections(
      {
        ...primary,
        model_id: modelId,
        route: buildRouteForModel(primaryProvider, modelId, primary.route),
      },
      fallbacks,
    )
  }

  const changePrimaryRoute = (routeId: string) => {
    const modelEntry = findModelEntry(primaryProvider, primaryModel)
    if (routeId === CUSTOM_ROUTE) {
      updateSelections(
        {
          ...primary,
          route: buildCustomRoute(primary.route),
        },
        fallbacks,
      )
      return
    }
    const endpoint = modelEntry?.endpoints?.find((ep) => ep.id === routeId)
    if (!endpoint) return
    updateSelections(
      {
        ...primary,
        route: buildRouteFromEndpoint(primaryProvider, endpoint),
      },
      fallbacks,
    )
  }

  const patchPrimaryCustomRoute = (patch: Partial<LlmRouteConfig>) => {
    updateSelections(
      {
        ...primary,
        route: patchCustomRoute(primary.route, patch),
      },
      fallbacks,
    )
  }

  /** Change primary API key value. */
  const changePrimaryKey = (value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[primaryEnv] = value
    } else {
      delete newEnvVars[primaryEnv]
    }
    updateConfig({
      env_vars: newEnvVars,
      llm: {
        primary: {
          ...primary,
          route: {
            ...(primary.route || {}),
            api_key_env: primaryEnv,
          },
        },
        fallbacks,
      },
    })
  }

  const setFallbacks = (fbs: LlmModelSelectionConfig[]) => {
    updateSelections(primary, fbs)
  }

  const addFallback = () => {
    const provider = 'deepseek'
    const models = getModelIds(provider, fetchedModels)
    const env = getFallbackEnvName(provider, 0, fallbacks, primaryEnv)
    setFallbacks([{
      family_id: provider,
      model_id: models[0] || null,
      route: {
        route_id: OFFICIAL_ROUTE_ID,
        label: 'Official API',
        base_url: null,
        api_key_env: env,
        api_type: null,
      },
    }, ...fallbacks])
    // Scroll to the new fallback after render
    setTimeout(() => document.getElementById('fallback-0')?.scrollIntoView({ behavior: 'smooth', block: 'center' }), 100)
  }

  /** Change fallback provider — updates provider, model, and api_key_env together. */
  const changeFallbackProvider = (idx: number, provider: string) => {
    const modelId = getModelIds(provider, fetchedModels)[0] || null
    const nextRoute = buildRouteForModel(provider, modelId, null)
    const envName = nextRoute.api_key_env || getFallbackEnvName(provider, idx, fallbacks, primaryEnv)
    updateFallback(idx, {
      family_id: provider,
      model_id: modelId,
      route: {
        route_id: nextRoute.route_id,
        label: nextRoute.label,
        api_key_env: envName,
        base_url: nextRoute.base_url,
        api_type: null,
      },
    })
  }

  /** Change fallback model while preserving the current custom or matching route when possible. */
  const changeFallbackModel = (idx: number, modelId: string) => {
    const fb = fallbacks[idx]
    if (!fb) return
    const nextRoute = buildRouteForModel(fb.family_id, modelId, fb.route)
    const envName = nextRoute.api_key_env || getFallbackEnvName(fb.family_id || '', idx, fallbacks, primaryEnv)
    updateFallback(idx, {
      model_id: modelId,
      route: {
        route_id: nextRoute.route_id,
        label: nextRoute.label,
        api_key_env: envName,
        base_url: nextRoute.base_url,
        api_type: null,
      },
    })
  }

  const changeFallbackRoute = (idx: number, routeId: string) => {
    const fb = fallbacks[idx]
    if (!fb) return
    if (routeId === CUSTOM_ROUTE) {
      updateFallback(idx, {
        route: buildCustomRoute(fb.route),
      })
      return
    }
    const modelEntry = findModelEntry(fb.family_id, fb.model_id)
    const endpoint = modelEntry?.endpoints?.find((ep) => ep.id === routeId)
    if (!endpoint) return
    updateFallback(idx, {
      route: buildRouteFromEndpoint(fb.family_id, endpoint),
    })
  }

  const patchFallbackCustomRoute = (idx: number, patch: Partial<LlmRouteConfig>) => {
    const fb = fallbacks[idx]
    if (!fb) return
    updateFallback(idx, {
      route: patchCustomRoute(fb.route, patch),
    })
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

  const updateFallback = (idx: number, patch: Partial<LlmModelSelectionConfig>) => {
    const updated = fallbacks.map((fb, i) => (i === idx ? { ...fb, ...patch } : fb))
    updateSelections(primary, updated)
  }

  const updateFallbackEnvVar = (idx: number, fbEnv: string, value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[fbEnv] = value
    } else {
      delete newEnvVars[fbEnv]
    }
    const updated = fallbacks.map((fb, i) => (
      i === idx
        ? {
            ...fb,
            route: {
              ...(fb.route || {}),
              api_key_env: fbEnv,
            },
          }
        : fb
    ))
    onChange({
      ...applySelections(config, primary, updated),
      env_vars: newEnvVars,
    })
  }

  const doTest = async (
    key: number,
    provider: string,
    model: string,
    apiKeyEnv: string,
    baseUrl?: string | null,
    requiresBaseUrl?: boolean,
  ) => {
    const apiKey = config.env_vars[apiKeyEnv] || ''
    if (!apiKey) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: 'No API key configured.', pricing: null } }))
      return
    }
    if (!model) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: 'No model selected.', pricing: null } }))
      return
    }
    if ((!isKnownProvider(provider) || requiresBaseUrl) && !baseUrl) {
      setTestResults((s) => ({ ...s, [key]: { state: 'error', error: 'This custom route requires a Base URL.', pricing: null } }))
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
        base_url: baseUrl || undefined,
      })
      const pricing = getModelPricing(provider, model)
        if (res.ok) {
          setTestResults((s) => ({ ...s, [key]: { state: 'success', error: '', pricing } }))
          // Store dynamically fetched models from the provider
          if (res.models && res.models.length > 0) {
            setFetchedModels((s) => ({ ...s, [providerRouteKey(provider, baseUrl)]: res.models! }))
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
        The stored schema is model family → model name → route. Pick the family first, then the model, then the official API, AutoDL, WiseModel, or a custom OpenAI-compatible route for that model.
      </div>

      {/* ── Primary Provider ── */}
      <div className="bg-surface-dark/30 rounded-lg p-4 border border-gray-700/50 space-y-4">
        <h3 className="text-sm font-semibold text-gray-200">Primary LLM</h3>

        <ProviderSelect
          provider={primaryProvider}
          baseUrl={primaryBaseUrl}
          onProviderChange={(p) => changePrimaryProvider(p)}
          onBaseUrlChange={(url) => updateSelections(
            {
              ...primary,
              route: {
                ...(primary.route || {}),
                base_url: url || null,
                api_key_env: primary.route?.api_key_env || primaryEnv,
              },
            },
            fallbacks,
          )}
        />

        <ModelSelect
          provider={primaryProvider}
          model={primaryModel}
          onModelChange={(model) => changePrimaryModel(model)}
          baseUrl={primary.route?.base_url}
          fetchedModels={fetchedModels}
        />

        <EndpointSelect
          provider={primaryProvider}
          modelId={primaryModel}
          route={primary.route}
          onRouteChange={changePrimaryRoute}
          onCustomRouteChange={patchPrimaryCustomRoute}
        />

        <Field label="API Key" hint={primaryEnv ? `Stored as ${primaryEnv}` : undefined}>
          <input
            type="password"
            value={config.env_vars[primaryEnv] || ''}
            onChange={(e) => changePrimaryKey(e.target.value)}
            placeholder={`Paste your ${primaryRouteLabel.toLowerCase()} key`}
            className="input font-mono text-xs"
          />
        </Field>

        <TestButton
          result={testResults[-1] || null}
          onTest={() => doTest(
            -1,
            primaryProvider || 'anthropic',
            primaryModel || '',
            primaryEnv,
            primary.route?.base_url,
            hasCustomRoute(primaryProvider, primaryModel, primary.route),
          )}
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
          const fbProvider = fb.family_id || ''
          const fbModel = fb.model_id || ''
          const fbEnv = fb.route?.api_key_env || getApiKeyEnvName(fbProvider)
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

              <ProviderSelect
                provider={fbProvider}
                baseUrl={fb.route?.base_url || ''}
                onProviderChange={(p) => changeFallbackProvider(idx, p || '')}
                onBaseUrlChange={(url) => updateFallback(idx, {
                  route: {
                    ...(fb.route || {}),
                    base_url: url || null,
                    api_key_env: fbEnv,
                  },
                })}
              />

              <ModelSelect
                provider={fbProvider}
                model={fbModel}
                onModelChange={(model) => changeFallbackModel(idx, model)}
                baseUrl={fb.route?.base_url}
                fetchedModels={fetchedModels}
              />

              <EndpointSelect
                provider={fbProvider}
                modelId={fbModel}
                route={fb.route}
                onRouteChange={(routeId) => changeFallbackRoute(idx, routeId)}
                onCustomRouteChange={(patch) => patchFallbackCustomRoute(idx, patch)}
              />

              <Field label="API Key" hint={sharesPrimaryKey ? `Shared with primary (${fbEnv})` : `Stored as ${fbEnv}`}>
                <input
                  type="password"
                  value={config.env_vars[fbEnv] || ''}
                  onChange={(e) => updateFallbackEnvVar(idx, fbEnv, e.target.value)}
                  placeholder={sharesPrimaryKey ? 'Using primary API key' : `Paste your ${getRouteLabel(fbProvider, fbModel, fb.route).toLowerCase()} key`}
                  className="input font-mono text-xs"
                />
              </Field>

              <TestButton
                result={testResults[idx] || null}
                onTest={() => doTest(
                  idx,
                  fbProvider,
                  fbModel || '',
                  fbEnv,
                  fb.route?.base_url,
                  hasCustomRoute(fbProvider, fbModel, fb.route),
                )}
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

function ProviderSelect({ provider, baseUrl, onProviderChange, onBaseUrlChange }: {
  provider: string
  baseUrl: string
  onProviderChange: (p: string | null) => void
  onBaseUrlChange: (url: string) => void
}) {
  const known = isKnownProvider(provider)
  const isFullyCustom = !!provider && !known
  const [customSelected, setCustomSelected] = useState(isFullyCustom)

  useEffect(() => {
    if (isFullyCustom) setCustomSelected(true)
  }, [isFullyCustom])

  const selectValue = customSelected && !provider
    ? CUSTOM_FAMILY
    : !provider
      ? ''
      : known
        ? provider
        : CUSTOM_FAMILY

  return (
    <>
      <Field label="Model Family">
        <select
          value={selectValue}
          onChange={(e) => {
            const v = e.target.value
            if (v === CUSTOM_FAMILY) {
              setCustomSelected(true)
              onProviderChange('')
            } else {
              setCustomSelected(false)
              onProviderChange(v || null)
            }
          }}
          className="input"
        >
          {!provider && <option value="">Select a model family...</option>}
          {PROVIDER_FAMILIES.map((p) => (
            <option key={p} value={p}>{p}</option>
          ))}
          <option value={CUSTOM_FAMILY}>
            {isFullyCustom ? `Custom family: ${provider}` : 'Custom model family…'}
          </option>
        </select>
      </Field>
      {(isFullyCustom || customSelected) && (
        <>
          <Field label="Family ID" hint="Only use this when the model family is not in the built-in catalog.">
            <input
              value={provider}
              onChange={(e) => onProviderChange(e.target.value)}
              placeholder="my-family"
              className="input text-xs font-mono"
            />
          </Field>
          <Field label="Base URL" hint="Default OpenAI-compatible API URL for this custom family.">
            <input
              value={baseUrl}
              onChange={(e) => onBaseUrlChange(e.target.value)}
              placeholder="https://example.com/v1"
              className="input text-xs font-mono"
            />
          </Field>
        </>
      )}
    </>
  )
}

/** Render a per-model endpoint picker if the selected model has preconfigured routes. */
function EndpointSelect({ provider, modelId, route, onRouteChange, onCustomRouteChange }: {
  provider: string
  modelId: string
  route: LlmRouteConfig | null | undefined
  onRouteChange: (routeId: string) => void
  onCustomRouteChange: (patch: Partial<LlmRouteConfig>) => void
}) {
  const model = findModelEntry(provider, modelId)
  const list = model?.endpoints ?? []
  const isCustom = hasCustomRoute(provider, modelId, route)
  if (list.length === 0 && !isCustom) return null

  const matched = findMatchingEndpoint(model, route)
  const value = isCustom ? CUSTOM_ROUTE : (matched?.id ?? list[0]?.id ?? CUSTOM_ROUTE)

  return (
    <div className="space-y-3">
      {list.length > 0 && (
        <Field label="Route / API Provider" hint="The family and model stay fixed. This chooses the actual API host and credential set.">
          <select
            value={value}
            onChange={(e) => onRouteChange(e.target.value)}
            className="input"
          >
            {list.map((ep) => (
              <option key={ep.id} value={ep.id}>
                {ep.label}{ep.base_url ? ` — ${ep.base_url}` : ''}
              </option>
            ))}
            <option value={CUSTOM_ROUTE}>Custom route…</option>
          </select>
        </Field>
      )}

      {(isCustom || list.length === 0) && (
        <div className="space-y-3 rounded-lg border border-gray-700/40 bg-surface-dark/20 p-3">
          <Field label="Route Name" hint="Name of the API provider route, such as AutoDL, WiseModel, or your own host.">
            <input
              value={route?.label || ''}
              onChange={(e) => onCustomRouteChange({ label: e.target.value })}
              placeholder="My custom route"
              className="input text-xs font-mono"
            />
          </Field>
          <Field label="Route Base URL" hint="OpenAI-compatible API URL for this route.">
            <input
              value={route?.base_url || ''}
              onChange={(e) => onCustomRouteChange({ base_url: e.target.value })}
              placeholder="https://example.com/v1"
              className="input text-xs font-mono"
            />
          </Field>
        </div>
      )}
    </div>
  )
}

function ModelSelect({ provider, model, baseUrl, onModelChange, fetchedModels }: { provider: string; model: string; baseUrl?: string | null; onModelChange: (m: string) => void; fetchedModels?: Record<string, string[]> }) {
  const entry = PROVIDER_MODELS[provider || '']
  const staticModels = entry?.models || []
  const staticIds = staticModels.map((m) => m.id)
  // Merge static + dynamically fetched models
  const dynamicIds = (fetchedModels?.[providerRouteKey(provider, baseUrl)] || []).filter((id) => !staticIds.includes(id))
  const allIds = [...staticIds, ...dynamicIds]
  const isCustom = model !== '' && !allIds.includes(model)
  const [customSelected, setCustomSelected] = useState(isCustom)
  useEffect(() => {
    if (isCustom) setCustomSelected(true)
    else if (model && allIds.includes(model)) setCustomSelected(false)
  }, [allIds, isCustom, model])
  const pricing = staticModels.find((m) => m.id === model)
  const endpointSummary = formatEndpointSummary(pricing)

  const selectValue = (isCustom || (customSelected && !model)) ? CUSTOM_MODEL : model

  return (
    <Field label="Model Name">
      <div className="space-y-2">
        <select
          value={selectValue}
          onChange={(e) => {
            if (e.target.value === CUSTOM_MODEL) {
              setCustomSelected(true)
              onModelChange('')
            } else {
              setCustomSelected(false)
              onModelChange(e.target.value)
            }
          }}
          className="input"
        >
          {!model && <option value="">Select a model...</option>}
          {staticModels.map((m) => (
            <option key={m.id} value={m.id}>{m.id}</option>
          ))}
          {dynamicIds.length > 0 && <option disabled>── fetched from API ──</option>}
          {dynamicIds.map((id) => (
            <option key={id} value={id}>{id}</option>
          ))}
          <option value={CUSTOM_MODEL}>{isCustom ? `Custom: ${model}` : 'Custom model…'}</option>
        </select>
        {(isCustom || customSelected || (!model && allIds.length === 0)) && (
          <input
            value={model}
            onChange={(e) => {
              setCustomSelected(true)
              onModelChange(e.target.value)
            }}
            placeholder="Enter the exact model ID"
            className="input text-xs font-mono"
            autoFocus
          />
        )}
        {pricing && pricing.input > 0 && (
          <p className="text-xs text-gray-500">
            {formatPrice(pricing)}
          </p>
        )}
        {endpointSummary && (
          <p className="text-xs text-gray-500">
            Built-in routes: {endpointSummary}
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
