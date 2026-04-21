import { useState } from 'react'
import { useProfile } from '../../contexts/ProfileContext'
import CategoryTabs from '../../components/CategoryTabs'
import SaveFooter from '../../components/SaveFooter'
import SearchApiTab from '../../components/tabs/SearchApiTab'
import EmailTab from '../../components/tabs/EmailTab'
import DeepCrawlTab from '../../components/tabs/DeepCrawlTab'

const TABS = [
  { key: 'search', label: 'Search APIs' },
  { key: 'email', label: 'Email' },
  { key: 'crawl', label: 'Deep Crawl' },
]

export default function ToolsPage() {
  const { profileId, config, setConfig, save, saving, loading } = useProfile()
  const [activeTab, setActiveTab] = useState('search')

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  return (
    <div>
      <h1 className="text-2xl font-bold text-white mb-6">Tools</h1>
      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
        <div className="px-5 pt-4">
          <CategoryTabs tabs={TABS} activeTab={activeTab} onTabChange={setActiveTab} />
        </div>
        <div className="p-5">
          {activeTab === 'search' && <SearchApiTab config={config} onChange={setConfig} profileId={profileId} />}
          {activeTab === 'email' && <EmailTab config={config} onChange={setConfig} />}
          {activeTab === 'crawl' && <DeepCrawlTab config={config} onChange={setConfig} />}
        </div>
        <SaveFooter onSave={save} saving={saving} />
      </div>
    </div>
  )
}
