import { useEffect, useState } from 'react'
import LlmProviderTab from '../../components/tabs/LlmProviderTab'
import type { ProfileConfig } from '../../types'

export const WIZARD_DRAFT_KEY = 'wizard.profile.draft'

const DEFAULT_DRAFT: ProfileConfig = {
  channels: [],
  gateway: {},
  env_vars: {},
}

function loadDraft(): ProfileConfig {
  try {
    const raw = sessionStorage.getItem(WIZARD_DRAFT_KEY)
    if (raw) return { ...DEFAULT_DRAFT, ...JSON.parse(raw) }
  } catch {
    // ignore parse failure
  }
  return DEFAULT_DRAFT
}

export default function StepLlmProvider() {
  const [config, setConfig] = useState<ProfileConfig>(loadDraft)

  useEffect(() => {
    try {
      sessionStorage.setItem(WIZARD_DRAFT_KEY, JSON.stringify(config))
    } catch {
      // ignore quota errors
    }
  }, [config])

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">LLM Provider</h2>
        <p className="text-sm text-gray-400">
          Pick a default model provider and verify your API key works. Your selections
          will pre-fill on the New Profile screen at the end of the wizard.
        </p>
      </div>
      <LlmProviderTab config={config} onChange={setConfig} />
    </div>
  )
}
