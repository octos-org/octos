import { useState } from 'react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import LlmProviderTab from './LlmProviderTab'
import providers from '../../providers.json'
import type { ProfileConfig, LlmRouteConfig } from '../../types'

const mocks = vi.hoisted(() => ({ changed: vi.fn() }))
vi.mock('../../api', () => ({ myApi: { fetchProviderModels: vi.fn().mockResolvedValue([]) } }))
const privateRoute: LlmRouteConfig = {
  route_id: null, label: 'Private proxy', base_url: 'https://private.example/v1',
  api_key_env: 'PRIVATE_PROXY_KEY', api_type: 'chat_completions',
}
type Target = 'primary' | 'fallback'
function mount(target: Target, family: string, model: string) {
  const selection = { family_id: family, model_id: model, route: privateRoute }
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
