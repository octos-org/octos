import { useState } from 'react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import LlmProviderTab from './LlmProviderTab'
import providers from '../../providers.json'
import type { ProfileConfig, LlmRouteConfig } from '../../types'

const mocks = vi.hoisted(() => ({ changed: vi.fn() }))
vi.mock('../../api', () => ({ myApi: { fetchProviderModels: vi.fn().mockResolvedValue([]) } }))
vi.mock('../../providers.json', async (importOriginal) => {
  const actual = await importOriginal<{ default: typeof providers }>()
  return { default: {
    ...actual.default,
    deepseek: { ...actual.default.deepseek, models: [
      ...actual.default.deepseek.models,
      { id: 'test-official-only', input: 1, output: 1, endpoints: [
        { id: 'deepseek', label: 'Official API' },
      ] },
    ] },
  } }
})
const privateRoute: LlmRouteConfig = {
  route_id: null, label: 'Private proxy', base_url: 'https://private.example/v1',
  api_key_env: 'PRIVATE_PROXY_KEY', api_type: 'chat_completions',
}
const autoDlRoute: LlmRouteConfig = {
  route_id: 'autodl', label: 'AutoDL', base_url: 'https://www.autodl.art/api/v1',
  api_key_env: 'AUTODL_API_KEY', api_type: null,
}
const officialRoute: LlmRouteConfig = {
  route_id: 'deepseek', label: 'Official API', base_url: null,
  api_key_env: 'DEEPSEEK_API_KEY', api_type: null,
}
type Target = 'primary' | 'fallback'
function mount(target: Target, family: string, model: string, route = privateRoute) {
  const selection = { family_id: family, model_id: model, route }
  const initial = { channels: [], env_vars: { PRIVATE_PROXY_KEY: 'dummy' }, gateway: {},
    llm: { primary: target === 'primary' ? selection : {
      family_id: 'anthropic', model_id: providers.anthropic.models[0].id, route: {},
    }, fallbacks: target === 'fallback' ? [selection] : [] } } as ProfileConfig
  function Harness() {
    const [config, setConfig] = useState(initial)
    return <LlmProviderTab config={config} onChange={next => { mocks.changed(next); setConfig(next) }} />
  }
  render(<Harness />)
}
function current(target: Target) {
  const config = mocks.changed.mock.lastCall![0] as ProfileConfig
  return target === 'primary' ? config.llm!.primary! : config.llm!.fallbacks![0]
}
function selectWithOption(value: string) {
  return screen.getAllByRole('combobox').find(element =>
    Array.from((element as HTMLSelectElement).options).some(option => option.value === value),
  )!
}
beforeEach(() => { mocks.changed.mockClear() })

describe('LLM model edits preserve route configuration', () => {
  for (const target of ['primary', 'fallback'] as const) {
    for (const route of [autoDlRoute, { ...autoDlRoute, route_id: null }]) {
      it(`resets an unsupported ${target} catalog route (${route.route_id || 'legacy URL'}) and displays the saved endpoint`, async () => {
        const user = userEvent.setup()
        mount(target, 'deepseek', 'deepseek-v4-pro', route)
        await user.selectOptions(selectWithOption('test-official-only'), 'test-official-only')
        expect(current(target).route).toEqual(officialRoute)
        expect(screen.getByDisplayValue('Official API')).toHaveValue('deepseek')
      })
    }

    it(`preserves a supported ${target} catalog route and displays the saved endpoint`, async () => {
      const user = userEvent.setup()
      mount(target, 'deepseek', 'deepseek-v4-pro', autoDlRoute)
      await user.selectOptions(selectWithOption('deepseek-v4-flash'), 'deepseek-v4-flash')
      expect(current(target).route).toEqual(autoDlRoute)
      expect(selectWithOption('autodl')).toHaveValue('autodl')
    })

    it(`resets an unsupported ${target} catalog route to the family default when the new model has no endpoint list`, async () => {
      const user = userEvent.setup()
      mount(target, 'moonshot', 'kimi-k2.5', autoDlRoute)
      await user.selectOptions(selectWithOption('kimi-k2.6'), 'kimi-k2.6')
      expect(current(target).route).toEqual({
        route_id: null, label: 'Official API', base_url: null,
        api_key_env: 'MOONSHOT_API_KEY', api_type: null,
      })
      expect(screen.queryByRole('option', { name: /AutoDL/ })).not.toBeInTheDocument()
    })

    for (const [originalRoute, field, value] of [
      [autoDlRoute, 'base_url', privateRoute.base_url],
      [autoDlRoute, 'api_key_env', privateRoute.api_key_env],
      [officialRoute, 'api_key_env', privateRoute.api_key_env],
    ] as const) {
      it(`preserves a custom ${target} ${field} even with catalog route ID ${originalRoute.route_id} and displays it as custom`, async () => {
        const user = userEvent.setup()
        const route = { ...originalRoute, [field]: value }
        mount(target, 'deepseek', 'deepseek-v4-pro', route)
        expect(screen.getByRole('option', { name: /^Custom:/ })).toHaveProperty('selected', true)
        await user.selectOptions(selectWithOption('test-official-only'), 'test-official-only')
        expect(current(target).route).toEqual(route)
        expect(screen.getByRole('option', { name: /^Custom:/ })).toHaveProperty('selected', true)
      })
    }

    it(`preserves a custom ${target} protocol when the old catalog endpoint is unavailable`, async () => {
      const user = userEvent.setup()
      const route = { ...autoDlRoute, api_type: 'chat_completions' }
      mount(target, 'deepseek', 'deepseek-v4-pro', route)
      await user.selectOptions(selectWithOption('test-official-only'), 'test-official-only')
      expect(current(target).route).toEqual(route)
      expect(screen.getByRole('option', { name: /^Custom:/ })).toHaveProperty('selected', true)
    })

    it(`preserves every custom ${target} route field during free-text typing`, async () => {
      const user = userEvent.setup()
      mount(target, 'custom-family', 'model-a')
      await user.type(screen.getByDisplayValue('model-a'), 'bc')
      expect(current(target).model_id).toBe('model-abc')
      expect(current(target).route).toEqual(privateRoute)
    })

    it(`preserves a private ${target} endpoint when choosing another catalog model`, async () => {
      const user = userEvent.setup()
      const models = providers.deepseek.models
      mount(target, 'deepseek', models[0].id)
      await user.selectOptions(selectWithOption(models[1].id), models[1].id)
      expect(current(target).model_id).toBe(models[1].id)
      expect(current(target).route).toEqual(privateRoute)
    })

    it(`still resets defaults when explicitly changing the ${target} provider`, async () => {
      const user = userEvent.setup()
      mount(target, 'deepseek', providers.deepseek.models[0].id)
      const provider = screen.getAllByRole('combobox').find(element =>
        (element as HTMLSelectElement).value === 'deepseek',
      )!
      await user.selectOptions(provider, 'openai')
      expect(current(target).family_id).toBe('openai')
      expect(current(target).route?.base_url).toBeNull()
      expect(current(target).route?.api_key_env).toBe(providers.openai.env)
      expect(current(target).route?.api_type).toBeNull()
    })

    it(`still applies an explicitly selected ${target} catalog endpoint`, async () => {
      const user = userEvent.setup()
      mount(target, 'deepseek', 'deepseek-v4-pro')
      await user.selectOptions(selectWithOption('autodl'), 'autodl')
      expect(current(target).route).toEqual({
        route_id: 'autodl', label: 'AutoDL', base_url: 'https://www.autodl.art/api/v1',
        api_key_env: 'AUTODL_API_KEY', api_type: null,
      })
    })
  }
})
