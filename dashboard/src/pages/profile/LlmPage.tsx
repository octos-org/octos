import { useProfile } from '../../contexts/ProfileContext'
import LlmProviderTab from '../../components/tabs/LlmProviderTab'
import SaveFooter from '../../components/SaveFooter'

export default function LlmPage() {
  const { profileId, config, setConfig, save, saving, loading } = useProfile()

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  return (
    <div>
      <h1 className="text-2xl font-bold text-white mb-6">LLM Providers</h1>
      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
        <div className="p-5">
          <LlmProviderTab config={config} onChange={setConfig} profileId={profileId} />
        </div>
        <SaveFooter onSave={save} saving={saving} />
      </div>
    </div>
  )
}
